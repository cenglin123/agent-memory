//! Stage 3 S2: the read-only source and evidence gate.
//!
//! Every case uses a synthetic temporary archive root and a synthetic temporary
//! learning root. Nothing here reads a real session root, a real archive, a
//! real learning root or a recall configuration.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::archive::{
    hex_sha256, Archive, Manifest, ManifestObject, ManifestRecord, ManifestSource, RecordRef,
    SourceFingerprint, MANIFEST_SCHEMA,
};
use agent_memory::capture::capture_source;
use agent_memory::learning::gate::{
    self, GateReport, E_GENERATION_MISMATCH, E_PROVENANCE_MISMATCH, E_REF_FORMAT, E_REF_UNRESOLVED,
    E_SPAN_MISMATCH, E_TAINT_POLICY,
};
use agent_memory::learning::model::validate_candidate;
use agent_memory::learning::provenance::CLAUDE_PROVIDER;
use agent_memory::learning::{LearningStore, DB_FILE};

const DOMAIN: &str = "default";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Fixture {
    temp: tempfile::TempDir,
    archive: Archive,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp root");
        let archive = Archive::init(temp.path()).expect("archive init");
        Self { temp, archive }
    }

    fn learning_root(&self) -> PathBuf {
        self.temp.path().join("learning")
    }

    fn db_path(&self) -> PathBuf {
        self.learning_root().join(DB_FILE)
    }

    /// Write synthetic Claude Code JSONL lines and capture them.
    fn capture_claude(&self, lines: &[&str]) -> PathBuf {
        let path = self.temp.path().join("projects").join("session.jsonl");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create source dir");
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&path, body).expect("write source");
        let report = capture_source(&self.archive, &path, "claude", DOMAIN, None).expect("capture");
        assert!(report.ok, "capture must succeed: {:?}", report.error);
        path
    }
}

#[derive(Clone, Copy)]
struct Decl {
    session_kind: &'static str,
    origin_class: &'static str,
    evidence_kind: &'static str,
    recall_taint: bool,
}

impl Decl {
    fn new(
        session_kind: &'static str,
        origin_class: &'static str,
        evidence_kind: &'static str,
    ) -> Self {
        Self {
            session_kind,
            origin_class,
            evidence_kind,
            recall_taint: false,
        }
    }

    fn tainted(
        session_kind: &'static str,
        origin_class: &'static str,
        evidence_kind: &'static str,
    ) -> Self {
        Self {
            session_kind,
            origin_class,
            evidence_kind,
            recall_taint: true,
        }
    }
}

const OWNER_STATEMENT: Decl = Decl {
    session_kind: "interactive",
    origin_class: "owner",
    evidence_kind: "user_statement",
    recall_taint: false,
};

fn evidence_with_span(sha: &str, span: &str, generation: &str, decl: Decl) -> Value {
    json!({
        "ref": format!("archive-record:{sha}"),
        "span_sha256": span,
        "archive_generation": generation,
        "observed_at": "2026-11-01T00:00:00Z",
        "actor": "owner",
        "session_kind": decl.session_kind,
        "origin_class": decl.origin_class,
        "evidence_kind": decl.evidence_kind,
        "recall_taint": decl.recall_taint
    })
}

fn evidence(sha: &str, decl: Decl) -> Value {
    evidence_with_span(sha, sha, "1", decl)
}

fn candidate(evidence: Vec<Value>, contradicting: Vec<Value>) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": "Every learning evidence item points at a committed archive record",
        "target_kind": "rule",
        "proposed_target": "AGENTS.md#evidence-gate",
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": evidence,
        "contradicting_evidence": contradicting,
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
        "source_checkpoint": "checkpoint-gate",
        "extractor": "extractor-gate",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn run_gate(fixture: &Fixture, evidence: Vec<Value>, contradicting: Vec<Value>) -> GateReport {
    let value = candidate(evidence, contradicting);
    let validated = validate_candidate(&value).expect("draft must satisfy the v1 schema");
    gate::evaluate(&validated.record, &fixture.archive, DOMAIN).expect("gate runs")
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

fn count(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).expect("open learning.db");
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count rows")
}

fn tree(root: &Path) -> Vec<(String, u64)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
        let entries = std::fs::read_dir(dir).expect("read dir");
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let name = path
                    .strip_prefix(base)
                    .expect("prefix")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((name, entry.metadata().expect("metadata").len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// 1. Positive path: a plain user record is owner/interactive/user_statement.
// ---------------------------------------------------------------------------

const PLAIN_USER: &str = r#"{"type":"user","message":{"role":"user","content":"Always use explicit absolute roots"},"sessionId":"s-1"}"#;

/// A second line, captured by a later round: its record lives in the journal
/// only (implementation-006), never in the canonical ledger file.
const PLAIN_SECOND_USER: &str = r#"{"type":"user","message":{"role":"user","content":"A second directive, appended later"},"sessionId":"s-1"}"#;

#[test]
fn positive_plain_user_text_passes_as_owner_evidence() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());

    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.counts.passed, 1);
    assert_eq!(report.counts.failed, 0);
    assert!(report.owner_backed);
    assert!(report.candidate_id.starts_with("cand_"));

    let item = &report.items[0];
    assert_eq!(item.list, "evidence");
    assert_eq!(item.index, 0);
    assert_eq!(item.reference, format!("archive-record:{sha}"));
    assert_eq!(item.verdict, "passed");
    assert!(item.error_code.is_none());
    assert_eq!(item.facts.provider.as_deref(), Some(CLAUDE_PROVIDER));
    assert_eq!(item.facts.record_type.as_deref(), Some("user"));
    assert_eq!(item.facts.is_meta, Some(false));
    assert_eq!(item.facts.is_sidechain, Some(false));
    assert_eq!(item.facts.tool_result, Some(false));
    assert_eq!(item.facts.parse_ok, Some(true));
}

/// The gate resolves its record index through the merged ledger view, so a
/// record appended after the last compaction is visible without running one.
#[test]
fn a_record_that_only_lives_in_the_journal_passes_the_gate() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    // Second round, same source file, one new line: append only.
    fixture.capture_claude(&[PLAIN_USER, PLAIN_SECOND_USER]);
    let sha = hex_sha256(PLAIN_SECOND_USER.as_bytes());

    let canonical = std::fs::read_to_string(
        fixture.temp.path().join("state").join("records.json"),
    )
    .expect("canonical ledger");
    assert!(
        !canonical.contains(&sha),
        "the fixture record must live in the journal only"
    );
    let journal = std::fs::read_to_string(
        fixture.temp.path().join("state").join("records.jsonl"),
    )
    .expect("journal");
    assert!(journal.contains(&sha), "the record is in the journal");

    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.counts.passed, 1);
    assert_eq!(report.items[0].facts.provider.as_deref(), Some(CLAUDE_PROVIDER));
    assert_eq!(report.items[0].facts.record_type.as_deref(), Some("user"));
}

// ---------------------------------------------------------------------------
// 2. E_REF_FORMAT: only `archive-record:<64 lowercase hex>` is accepted.
// ---------------------------------------------------------------------------

#[test]
fn non_archive_record_references_are_format_errors() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());

    for reference in [
        "claude:session-x:line-1".to_string(),
        format!("object:{sha}"),
        format!("archive-record:{}", sha.to_uppercase()),
        format!("archive-record:{}", &sha[..63]),
        format!("archive-record:{}z", &sha[..63]),
    ] {
        let mut item = evidence(&sha, OWNER_STATEMENT);
        item["ref"] = Value::String(reference.clone());
        let report = run_gate(&fixture, vec![item], Vec::new());
        assert!(!report.ok, "reference {reference:?} must be rejected");
        assert_eq!(report.items[0].verdict, "failed");
        assert_eq!(report.items[0].error_code, Some(E_REF_FORMAT));
        // Nothing was read, so no fact is claimed.
        assert!(report.items[0].facts.provider.is_none());
        assert!(report.items[0].facts.parse_ok.is_none());
        assert!(!report.owner_backed);
    }
}

// ---------------------------------------------------------------------------
// 3. E_REF_UNRESOLVED: forged shas and uncommitted live-source tails.
// ---------------------------------------------------------------------------

#[test]
fn forged_and_uncommitted_live_tail_references_are_unresolved() {
    let fixture = Fixture::new();
    let source = fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());

    // A well-formed but never committed sha.
    let forged = "d".repeat(64);
    let report = run_gate(
        &fixture,
        vec![evidence(&forged, OWNER_STATEMENT)],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_REF_UNRESOLVED));

    // The live source grew after capture: the appended line exists on disk but
    // was never committed, so it is not evidence yet.
    let tail =
        r#"{"type":"user","message":{"role":"user","content":"late live tail"},"sessionId":"s-1"}"#;
    let mut body = std::fs::read_to_string(&source).expect("read source");
    body.push_str(tail);
    body.push('\n');
    std::fs::write(&source, body).expect("append tail");
    let tail_sha = hex_sha256(tail.as_bytes());
    let report = run_gate(
        &fixture,
        vec![evidence(&tail_sha, OWNER_STATEMENT)],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_REF_UNRESOLVED));

    // The committed record still resolves.
    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert!(report.ok);
}

// ---------------------------------------------------------------------------
// 4. E_SPAN_MISMATCH: a tampered span.
// ---------------------------------------------------------------------------

#[test]
fn tampered_span_is_rejected() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());
    let other_span = hex_sha256(b"some other bytes");

    let report = run_gate(
        &fixture,
        vec![evidence_with_span(&sha, &other_span, "1", OWNER_STATEMENT)],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_SPAN_MISMATCH));
    assert_eq!(report.items[0].facts.parse_ok, Some(true));
}

// ---------------------------------------------------------------------------
// 5. E_GENERATION_MISMATCH: a wrong committed generation.
// ---------------------------------------------------------------------------

#[test]
fn wrong_committed_generation_is_rejected() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());

    for generation in ["0", "2", "g1"] {
        let mut item = evidence(&sha, OWNER_STATEMENT);
        item["archive_generation"] = Value::String(generation.to_string());
        let report = run_gate(&fixture, vec![item], Vec::new());
        assert_eq!(
            report.items[0].error_code,
            Some(E_GENERATION_MISMATCH),
            "generation {generation:?}"
        );
    }

    // `1` is right.
    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert!(report.ok);
}

// ---------------------------------------------------------------------------
// 6. sidechain: subagent only.
// ---------------------------------------------------------------------------

const SIDECHAIN: &str = r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent note"},"sessionId":"s-1"}"#;

#[test]
fn sidechain_records_must_declare_subagent() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[SIDECHAIN]);
    let sha = hex_sha256(SIDECHAIN.as_bytes());

    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("interactive", "agent_derived", "unknown"),
        )],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_PROVENANCE_MISMATCH));
    assert_eq!(report.items[0].facts.is_sidechain, Some(true));

    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("subagent", "agent_derived", "unknown"),
        )],
        Vec::new(),
    );
    assert!(report.ok, "report: {report:?}");
    assert!(!report.owner_backed);
}

// ---------------------------------------------------------------------------
// 7. isMeta: runtime only, no owner testimony.
// ---------------------------------------------------------------------------

const META_USER: &str = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<system context>"},"sessionId":"s-1"}"#;

#[test]
fn meta_records_must_declare_runtime() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[META_USER]);
    let sha = hex_sha256(META_USER.as_bytes());

    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert_eq!(report.items[0].error_code, Some(E_PROVENANCE_MISMATCH));
    assert_eq!(report.items[0].facts.is_meta, Some(true));

    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("interactive", "runtime", "user_statement"),
        )],
        Vec::new(),
    );
    assert_eq!(
        report.items[0].error_code,
        Some(E_PROVENANCE_MISMATCH),
        "a meta record can never carry owner testimony"
    );

    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("interactive", "runtime", "unknown"),
        )],
        Vec::new(),
    );
    assert!(report.ok, "report: {report:?}");
}

// ---------------------------------------------------------------------------
// 8. assistant: agent_derived only.
// ---------------------------------------------------------------------------

const ASSISTANT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]},"sessionId":"s-1"}"#;

#[test]
fn assistant_records_must_declare_agent_derived() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[ASSISTANT]);
    let sha = hex_sha256(ASSISTANT.as_bytes());

    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert_eq!(report.items[0].error_code, Some(E_PROVENANCE_MISMATCH));

    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("interactive", "agent_derived", "unknown"),
        )],
        Vec::new(),
    );
    assert!(report.ok, "report: {report:?}");
    assert!(!report.owner_backed);
}

// ---------------------------------------------------------------------------
// 9. tool_result: restricted evidence.
// ---------------------------------------------------------------------------

const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t-1","content":"ok"}]},"sessionId":"s-1"}"#;

#[test]
fn tool_result_records_are_restricted_evidence() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[TOOL_RESULT]);
    let sha = hex_sha256(TOOL_RESULT.as_bytes());

    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert_eq!(report.items[0].error_code, Some(E_PROVENANCE_MISMATCH));
    assert_eq!(report.items[0].facts.tool_result, Some(true));

    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("interactive", "runtime", "execution_result"),
        )],
        Vec::new(),
    );
    assert!(report.ok, "report: {report:?}");

    // `approval` is still owner testimony and stays rejected.
    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("interactive", "runtime", "approval"),
        )],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_PROVENANCE_MISMATCH));
}

// ---------------------------------------------------------------------------
// 10. recall taint policy: two rejection paths.
// ---------------------------------------------------------------------------

#[test]
fn recall_taint_policy_has_two_rejection_paths() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());

    // (a) tainted material must be declared `recalled_material`.
    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::tainted("interactive", "owner", "user_statement"),
        )],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_TAINT_POLICY));

    // (b) tainted material may never be owner testimony.
    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::tainted("interactive", "owner", "recalled_material"),
        )],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_TAINT_POLICY));

    // A tainted recall item declared honestly passes.
    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::tainted("interactive", "external", "recalled_material"),
        )],
        Vec::new(),
    );
    assert!(report.ok, "report: {report:?}");
    assert!(!report.owner_backed);
}

// ---------------------------------------------------------------------------
// 11. Non-Claude provider: fail-closed.
// ---------------------------------------------------------------------------

const OPENCODE_RECORD: &str = r#"{"id":"msg-1","sessionID":"ses-1","role":"user","content":"hi"}"#;

/// Commit a synthetic record for a non-Claude provider (opencode/sqlite shape)
/// through the public manifest/record-index writers. The object holds the
/// application-level zstd payload (the SQLite delta-batch semantics: record
/// offsets address the decompressed bytes), exactly like the live collector.
fn synthetic_provider_record(fixture: &Fixture, provider: &str, bytes: &[u8]) -> String {
    let payload = zstd::stream::encode_all(bytes, 3).expect("compress batch");
    let (object_sha, _, _) = fixture.archive.put_object(&payload).expect("put object");
    let sha = hex_sha256(bytes);
    let len = bytes.len() as u64;
    let capture_id = format!("cap-synthetic-{provider}");
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_string(),
        capture_id: capture_id.clone(),
        captured_at_ms: 1_700_000_000_000,
        device_id: fixture.archive.device_id().expect("device id"),
        provider: provider.to_string(),
        source: ManifestSource {
            kind: "sqlite".to_string(),
            path: format!("synthetic://{provider}"),
            compressed: false,
            fingerprint: SourceFingerprint {
                mtime_ms: 0,
                len,
                head_sha256: hex_sha256(b""),
                tail_sha256: hex_sha256(b""),
            },
            source_state_at_capture: "present".to_string(),
            capture_reason: "initial".to_string(),
            generation: 1,
            range_start: 0,
            carrier: None,
            sqlite: None,
        },
        objects: vec![ManifestObject {
            sha256: object_sha.clone(),
            bytes: len,
            role: "sqlite_delta".to_string(),
            access: vec![DOMAIN.to_string()],
            source_offset: Some(0),
            generation: 1,
        }],
        records: vec![ManifestRecord {
            seq: 0,
            offset: 0,
            len,
            sha256: sha.clone(),
            object: object_sha.clone(),
            access: vec![DOMAIN.to_string()],
            source_offset: Some(0),
            source_line: Some(1),
        }],
        attachments: Vec::new(),
    };
    fixture.archive.commit_manifest(&manifest).expect("commit");
    let mut records = fixture.archive.load_records().expect("records");
    records.records.insert(
        sha.clone(),
        RecordRef {
            object: object_sha,
            offset: 0,
            len,
            capture_id,
        },
    );
    fixture
        .archive
        .save_records(&records)
        .expect("save records");
    sha
}

#[test]
fn non_claude_provider_is_fail_closed() {
    let fixture = Fixture::new();
    let sha = synthetic_provider_record(&fixture, "opencode", OPENCODE_RECORD.as_bytes());

    let report = run_gate(
        &fixture,
        vec![evidence(
            &sha,
            Decl::new("interactive", "owner", "user_statement"),
        )],
        Vec::new(),
    );
    assert_eq!(report.items[0].error_code, Some(E_PROVENANCE_MISMATCH));
    assert_eq!(report.items[0].facts.provider.as_deref(), Some("opencode"));
    assert_eq!(report.items[0].facts.parse_ok, Some(false));
    assert!(report.items[0].facts.record_type.is_none());

    let report = run_gate(
        &fixture,
        vec![evidence(&sha, Decl::new("unknown", "unknown", "unknown"))],
        Vec::new(),
    );
    assert!(report.ok, "report: {report:?}");
    assert!(!report.owner_backed);
}

// ---------------------------------------------------------------------------
// 12. Unparsable record: all-unknown declarations only.
// ---------------------------------------------------------------------------

const MALFORMED: &str = r#"{"type":"user","message":"#;

#[test]
fn unparsable_record_only_accepts_unknown_declarations() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[MALFORMED]);
    let sha = hex_sha256(MALFORMED.as_bytes());

    let report = run_gate(&fixture, vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    assert_eq!(report.items[0].error_code, Some(E_PROVENANCE_MISMATCH));
    assert_eq!(
        report.items[0].facts.provider.as_deref(),
        Some(CLAUDE_PROVIDER)
    );
    assert_eq!(report.items[0].facts.parse_ok, Some(false));

    let report = run_gate(
        &fixture,
        vec![evidence(&sha, Decl::new("unknown", "unknown", "unknown"))],
        Vec::new(),
    );
    assert!(report.ok, "report: {report:?}");
}

// ---------------------------------------------------------------------------
// 13. owner_backed is a report flag over qualified owner evidence.
// ---------------------------------------------------------------------------

#[test]
fn owner_backed_reports_qualified_owner_evidence_only() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER, ASSISTANT]);
    let user_sha = hex_sha256(PLAIN_USER.as_bytes());
    let assistant_sha = hex_sha256(ASSISTANT.as_bytes());

    let report = run_gate(
        &fixture,
        vec![evidence(
            &assistant_sha,
            Decl::new("interactive", "agent_derived", "unknown"),
        )],
        Vec::new(),
    );
    assert!(report.ok);
    assert!(!report.owner_backed);

    // Counter-evidence is gated with the same rules; its owner testimony also
    // counts for the report flag (the gate never resolves contradictions).
    let report = run_gate(
        &fixture,
        vec![
            evidence(
                &assistant_sha,
                Decl::new("interactive", "agent_derived", "unknown"),
            ),
            evidence(&user_sha, OWNER_STATEMENT),
        ],
        vec![evidence(&user_sha, OWNER_STATEMENT)],
    );
    assert!(report.ok, "report: {report:?}");
    assert!(report.owner_backed);
    assert_eq!(report.counts.passed, 3);
    assert_eq!(report.items[1].list, "evidence");
    assert_eq!(report.items[2].list, "contradicting_evidence");
    assert_eq!(report.items[2].index, 0);
}

// ---------------------------------------------------------------------------
// 14. CLI: both input paths, exit codes, and the read-only contract.
// ---------------------------------------------------------------------------

#[test]
fn cli_gate_paths_are_read_only() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());

    let mut store = LearningStore::init(&fixture.learning_root()).expect("learning init");
    let draft = candidate(vec![evidence(&sha, OWNER_STATEMENT)], Vec::new());
    let outcome = store.ingest(&draft).expect("ingest");
    let candidate_id = outcome.candidate_id.clone();
    let draft_text = serde_json::to_string(&draft).expect("candidate json");
    let draft_file = fixture.temp.path().join("draft.json");
    std::fs::write(&draft_file, &draft_text).expect("write draft");
    drop(store);

    let before_counts = [
        count(&fixture.db_path(), "candidates"),
        count(&fixture.db_path(), "candidate_revisions"),
        count(&fixture.db_path(), "ingestions"),
        count(&fixture.db_path(), "status_history"),
    ];
    let before_mtime = std::fs::metadata(fixture.db_path())
        .expect("db metadata")
        .modified()
        .expect("mtime");
    let before_archive = tree(fixture.temp.path());

    // Path A: an already stored candidate, by id.
    let out = cli()
        .args(["learning", "gate", "--root"])
        .arg(fixture.learning_root())
        .args(["--archive"])
        .arg(fixture.temp.path())
        .args(["--domain", DOMAIN, "--candidate-id", candidate_id.as_str()])
        .output()
        .expect("run gate");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = stdout_json(&out);
    assert_eq!(report["ok"], true);
    assert_eq!(report["candidate_id"], candidate_id);
    assert_eq!(report["owner_backed"], true);
    assert_eq!(report["counts"]["passed"], 1);
    assert_eq!(report["counts"]["failed"], 0);
    assert_eq!(report["items"][0]["verdict"], "passed");
    assert_eq!(report["items"][0]["ref"], format!("archive-record:{sha}"));

    // Path B: an un-stored draft, inline and via @file.
    for candidate_arg in [draft_text.clone(), format!("@{}", draft_file.display())] {
        let out = cli()
            .args(["learning", "gate", "--root"])
            .arg(fixture.learning_root())
            .args(["--archive"])
            .arg(fixture.temp.path())
            .args(["--domain", DOMAIN, "--candidate", candidate_arg.as_str()])
            .output()
            .expect("run gate");
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let report = stdout_json(&out);
        assert_eq!(report["ok"], true);
        assert_eq!(report["counts"]["passed"], 1);
    }

    // Nothing was written: same rows, same mtime, same archive tree.
    let after_counts = [
        count(&fixture.db_path(), "candidates"),
        count(&fixture.db_path(), "candidate_revisions"),
        count(&fixture.db_path(), "ingestions"),
        count(&fixture.db_path(), "status_history"),
    ];
    assert_eq!(before_counts, after_counts);
    let after_mtime = std::fs::metadata(fixture.db_path())
        .expect("db metadata")
        .modified()
        .expect("mtime");
    assert_eq!(before_mtime, after_mtime, "learning.db mtime must not move");
    assert_eq!(before_archive, tree(fixture.temp.path()));

    // Unknown candidate id -> not found (6).
    let out = cli()
        .args(["learning", "gate", "--root"])
        .arg(fixture.learning_root())
        .args(["--archive"])
        .arg(fixture.temp.path())
        .args(["--domain", DOMAIN, "--candidate-id", "cand_missing"])
        .output()
        .expect("run gate");
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(stderr_json(&out)["error"]["code"], "CANDIDATE_NOT_FOUND");

    // Both inputs at once -> usage (2).
    let out = cli()
        .args(["learning", "gate", "--root"])
        .arg(fixture.learning_root())
        .args(["--archive"])
        .arg(fixture.temp.path())
        .args(["--domain", DOMAIN, "--candidate-id", candidate_id.as_str()])
        .args(["--candidate", draft_text.as_str()])
        .output()
        .expect("run gate");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_USAGE");
}

// ---------------------------------------------------------------------------
// 15. CLI: explicit roots and domain are mandatory and never inferred.
// ---------------------------------------------------------------------------

#[test]
fn cli_requires_explicit_absolute_roots_and_domain() {
    let fixture = Fixture::new();
    let root = fixture.learning_root();
    let archive = fixture.temp.path().to_path_buf();
    let draft = serde_json::to_string(&candidate(
        vec![evidence(&"a".repeat(64), OWNER_STATEMENT)],
        Vec::new(),
    ))
    .expect("candidate json");

    let cases: Vec<(Vec<String>, &str)> = vec![
        // Missing --archive.
        (
            vec![
                "learning".into(),
                "gate".into(),
                "--root".into(),
                root.display().to_string(),
                "--domain".into(),
                DOMAIN.into(),
                "--candidate".into(),
                draft.clone(),
            ],
            "INVALID_USAGE",
        ),
        // Missing --domain.
        (
            vec![
                "learning".into(),
                "gate".into(),
                "--root".into(),
                root.display().to_string(),
                "--archive".into(),
                archive.display().to_string(),
                "--candidate".into(),
                draft.clone(),
            ],
            "INVALID_USAGE",
        ),
        // Relative archive root.
        (
            vec![
                "learning".into(),
                "gate".into(),
                "--root".into(),
                root.display().to_string(),
                "--archive".into(),
                "runtime".into(),
                "--domain".into(),
                DOMAIN.into(),
                "--candidate".into(),
                draft.clone(),
            ],
            "GATE_ROOT_INVALID",
        ),
        // `~` archive root.
        (
            vec![
                "learning".into(),
                "gate".into(),
                "--root".into(),
                root.display().to_string(),
                "--archive".into(),
                "~/runtime".into(),
                "--domain".into(),
                DOMAIN.into(),
                "--candidate".into(),
                draft.clone(),
            ],
            "GATE_ROOT_INVALID",
        ),
        // Relative learning root.
        (
            vec![
                "learning".into(),
                "gate".into(),
                "--root".into(),
                "learning".into(),
                "--archive".into(),
                archive.display().to_string(),
                "--domain".into(),
                DOMAIN.into(),
                "--candidate".into(),
                draft.clone(),
            ],
            "LEARNING_ROOT_INVALID",
        ),
        // Empty domain.
        (
            vec![
                "learning".into(),
                "gate".into(),
                "--root".into(),
                root.display().to_string(),
                "--archive".into(),
                archive.display().to_string(),
                "--domain".into(),
                "  ".into(),
                "--candidate".into(),
                draft,
            ],
            "INVALID_USAGE",
        ),
    ];

    for (args, expected) in cases {
        let out = cli().args(&args).output().expect("run gate");
        assert_eq!(out.status.code(), Some(2), "args: {args:?}");
        assert_eq!(
            stderr_json(&out)["error"]["code"],
            expected,
            "args: {args:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Help surface.
// ---------------------------------------------------------------------------

#[test]
fn help_lists_the_gate_subcommand() {
    let out = cli().arg("--help").output().expect("run help");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("learning gate"), "help: {text}");
}

// ---------------------------------------------------------------------------
// Fail-closed top-level paths (fresh-verifier blind-spot closure).
// ---------------------------------------------------------------------------

#[test]
fn unreadable_committed_archive_state_fails_closed() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());
    // Archive::open only requires state/schema.json; the committed record
    // index is read inside the gate. Corrupting it must fail the whole gate
    // (integrity, exit 6), never produce a per-item verdict.
    std::fs::write(
        fixture.temp.path().join("state").join("records.json"),
        b"{broken",
    )
    .expect("corrupt records.json");

    let error = gate::evaluate(
        &validate_candidate(&candidate(
            vec![evidence(&sha, OWNER_STATEMENT)],
            Vec::new(),
        ))
        .expect("draft")
        .record,
        &fixture.archive,
        DOMAIN,
    )
    .expect_err("corrupt committed state must fail the gate");
    assert_eq!(error.code, "GATE_ARCHIVE_UNREADABLE");
    assert_eq!(error.exit_code(), 6);
}

#[test]
fn denied_access_domain_reports_unresolved() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());
    let record = validate_candidate(&candidate(
        vec![evidence(&sha, OWNER_STATEMENT)],
        Vec::new(),
    ))
    .expect("draft")
    .record;

    // The record is committed, but this domain is not in its access tags:
    // readback fails closed and the item is unresolved, not "passed".
    let report = gate::evaluate(&record, &fixture.archive, "other-domain").expect("gate runs");
    assert!(!report.ok);
    assert_eq!(report.counts.failed, 1);
    assert_eq!(report.items[0].verdict, "failed");
    assert_eq!(report.items[0].error_code, Some(E_REF_UNRESOLVED));
}

#[test]
fn first_failing_check_wins_in_combined_violations() {
    let fixture = Fixture::new();
    fixture.capture_claude(&[PLAIN_USER]);
    let sha = hex_sha256(PLAIN_USER.as_bytes());

    // Wrong span AND wrong generation: the span check (step 4) decides the
    // recorded code before the generation check (step 5).
    let report = run_gate(
        &fixture,
        vec![evidence_with_span(
            &sha,
            &"b".repeat(64),
            "9",
            OWNER_STATEMENT,
        )],
        Vec::new(),
    );
    assert!(!report.ok);
    assert_eq!(report.items[0].error_code, Some(E_SPAN_MISMATCH));
}
