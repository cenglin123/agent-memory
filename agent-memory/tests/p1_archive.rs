//! P1 acceptance tests for the evidence archive (headless, temp roots only).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_memory::archive::{hex_sha256, Archive, ArchiveError, Manifest};
use agent_memory::capture::{capture_source, Failpoint};

const DOMAIN: &str = "default";

fn codex_jsonl(dir: &Path, name: &str, extra_lines: &[String]) -> PathBuf {
    let mut lines = vec![
        "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"s-p1\",\"cwd\":\"/tmp/project\"}}".to_string(),
        "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hello p1\"}]}}".to_string(),
        "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]},\"x_future_field\":{\"nested\":[1,2,3]}}".to_string(),
    ];
    lines.extend(extra_lines.iter().cloned());
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn source_lines(path: &Path) -> Vec<Vec<u8>> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| l.as_bytes().to_vec())
        .collect()
}

fn init_root() -> (tempfile::TempDir, Archive) {
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(temp.path()).unwrap();
    (temp, archive)
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

// ---------------------------------------------------------------------------
// Test 2: first capture + repeat capture idempotence; unknown JSONL fields
// remain readable from the raw record bytes.
// ---------------------------------------------------------------------------
#[test]
fn capture_recapture_idempotent_and_unknown_fields_readable() {
    let (temp, archive) = init_root();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[]);

    let r1 = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(r1.status, "captured");
    assert_eq!(r1.objects_new, 1);
    assert_eq!(r1.records_new, 3);
    assert_eq!(r1.records_deduped, 0);
    assert!(r1.parse.error.is_none(), "parse probe: {:?}", r1.parse.error);

    let r2 = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(r2.status, "skipped_unchanged");
    assert_eq!(r2.objects_new, 0);
    assert_eq!(r2.records_new, 0);
    assert_eq!(archive.manifests().unwrap().len(), 1, "no duplicate manifest");
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);

    // The record carrying the unknown field is readable byte-exact.
    let lines = source_lines(&src);
    let unknown_line = &lines[2];
    let sha = hex_sha256(unknown_line);
    let handle = archive.reader(DOMAIN);
    let back = handle.read_record(&sha).unwrap();
    assert_eq!(&back, unknown_line);
    assert!(
        String::from_utf8(back).unwrap().contains("\"x_future_field\""),
        "unknown field must survive in the raw record bytes"
    );

    // Append a line -> new file object, per-record dedupe still holds.
    let extra = "{\"timestamp\":\"2026-03-06T21:50:15Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"more\"}]}}".to_string();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[extra]);
    let r3 = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(r3.status, "captured");
    assert_eq!(r3.records_new, 1);
    assert_eq!(r3.records_deduped, 3);
    assert_eq!(r3.objects_new, 1);
    assert_eq!(archive.manifests().unwrap().len(), 2);
    let rec = archive.reconcile(&[]).unwrap();
    assert_eq!(rec.objects_on_disk, 2);
    assert!(rec.orphan_objects.is_empty());
}

// ---------------------------------------------------------------------------
// Test 3: records beyond the cc-switch interactive read budget (32MB) archive
// byte-exact.
// ---------------------------------------------------------------------------
#[test]
fn large_record_above_read_budget_archives_byte_exact() {
    let (temp, archive) = init_root();
    let big_text = "x".repeat(33 * 1024 * 1024); // > 32MB interactive read budget
    let record = serde_json::json!({
        "timestamp": "2026-03-06T21:50:13Z",
        "type": "response_item",
        "payload": {"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": big_text}]}
    });
    let src = temp.path().join("rollout-big.jsonl");
    let mut f = std::fs::File::create(&src).unwrap();
    writeln!(f, "{}", serde_json::json!({"timestamp":"2026-03-06T21:50:12Z","type":"session_meta","payload":{"id":"s-big","cwd":"/tmp"}})).unwrap();
    writeln!(f, "{}", record).unwrap();
    drop(f);

    let source_bytes = std::fs::read(&src).unwrap();
    let r = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(r.status, "captured");
    assert_eq!(r.records_new, 2);

    let handle = archive.reader(DOMAIN);
    // Whole-file object is byte-identical to the source.
    let object_back = handle.read_object(&r.sha256.unwrap()).unwrap();
    assert_eq!(object_back, source_bytes);
    // The >32MB record itself is byte-identical.
    let big_line = source_lines(&src).remove(1);
    assert!(big_line.len() > 32 * 1024 * 1024);
    let record_back = handle.read_record(&hex_sha256(&big_line)).unwrap();
    assert_eq!(record_back, big_line);
}

// ---------------------------------------------------------------------------
// Test 4: crash between object write and manifest commit keeps the watermark
// untouched; rerun recovers; orphans are reported, never auto-deleted.
// ---------------------------------------------------------------------------
#[test]
fn crash_before_manifest_commit_recovers_and_reports_orphans() {
    let (temp, archive) = init_root();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[]);
    let crashed_sha = hex_sha256(&std::fs::read(&src).unwrap());

    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = capture_source(&archive, &src, "codex", DOMAIN, Some(Failpoint::BeforeManifestCommit));
    }));
    assert!(crashed.is_err(), "failpoint must fire");

    // Watermark not advanced; no manifest committed; the staged object is an orphan.
    let state = archive.load_sources().unwrap();
    assert!(state.sources.values().all(|s| s.watermark.is_none()));
    assert_eq!(archive.manifests().unwrap().len(), 0);
    let rec = archive.reconcile(&[]).unwrap();
    assert_eq!(rec.orphan_objects, vec![crashed_sha.clone()]);
    assert!(!archive.lock_held(), "lock released by unwinding");

    // Rerun after the source grew: capture succeeds, orphan stays reported.
    let extra = "{\"timestamp\":\"2026-03-06T21:50:15Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"recovered\"}]}}".to_string();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[extra]);
    let r = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(r.status, "captured");
    assert_eq!(r.records_new, 4);
    assert!(
        r.orphan_objects.contains(&crashed_sha),
        "orphan object from the crashed run must be reported, not deleted: {:?}",
        r.orphan_objects
    );
    let rec = archive.reconcile(&[]).unwrap();
    assert_eq!(rec.orphan_objects, vec![crashed_sha]);
    assert!(rec.missing_objects.is_empty());
}

// ---------------------------------------------------------------------------
// Test 5: after the source file is removed, readback still returns everything.
// ---------------------------------------------------------------------------
#[test]
fn source_deleted_after_capture_readback_still_complete() {
    let (temp, archive) = init_root();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[]);
    let source_bytes = std::fs::read(&src).unwrap();
    let lines = source_lines(&src);
    let r = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    let object_sha = r.sha256.unwrap();

    std::fs::remove_file(&src).unwrap();
    assert!(!src.exists());

    let handle = archive.reader(DOMAIN);
    let object_back = handle.read_object(&object_sha).unwrap();
    assert_eq!(object_back, source_bytes);
    assert_eq!(hex_sha256(&object_back), object_sha);
    for line in &lines {
        let back = handle.read_record(&hex_sha256(line)).unwrap();
        assert_eq!(&back, line);
    }
}

// ---------------------------------------------------------------------------
// Test 6: a second concurrent capture instance is explicitly refused.
// ---------------------------------------------------------------------------
#[test]
fn second_concurrent_capture_is_refused() {
    let (temp, archive) = init_root();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[]);

    let guard = archive.acquire_lock().unwrap();
    let err = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap_err();
    assert!(
        matches!(err, ArchiveError::LockHeld(_)),
        "expected LockHeld, got {err:?}"
    );
    drop(guard);
    let r = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(r.status, "captured");
}

// ---------------------------------------------------------------------------
// Test 7: fail-closed readback. Missing permission mapping denies; different
// permission domains cannot share a readback handle; unreferenced (orphan)
// objects deny every domain.
// ---------------------------------------------------------------------------
#[test]
fn readback_is_fail_closed_on_missing_or_foreign_access_mapping() {
    let (temp, archive) = init_root();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[]);
    let r = capture_source(&archive, &src, "codex", "alpha", None).unwrap();
    let object_sha = r.sha256.unwrap();

    // Same domain: allowed. Foreign domain: denied.
    assert!(archive.reader("alpha").read_object(&object_sha).is_ok());
    let err = archive.reader("beta").read_object(&object_sha).unwrap_err();
    assert!(matches!(err, ArchiveError::AccessDenied(_)), "got {err:?}");

    // Same content referenced again with a wider tag set: visibility is the
    // strictest intersection across all references (alpha only).
    let manifests = archive.manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    let mut wider: Manifest = manifests[0].clone();
    wider.capture_id = "cap-tampered-wider".to_string();
    for obj in &mut wider.objects {
        obj.access = vec!["alpha".to_string(), "beta".to_string()];
    }
    for rec in &mut wider.records {
        rec.access = vec!["alpha".to_string(), "beta".to_string()];
    }
    let wider_path = temp
        .path()
        .join("archive/manifests/99999998-cap-tampered-wider.json");
    std::fs::write(&wider_path, serde_json::to_string_pretty(&wider).unwrap()).unwrap();
    assert!(archive.reader("alpha").read_object(&object_sha).is_ok());
    let err = archive.reader("beta").read_object(&object_sha).unwrap_err();
    assert!(matches!(err, ArchiveError::AccessDenied(_)), "got {err:?}");

    // A reference with a missing permission mapping (empty tags) fails closed
    // for every domain.
    let mut unmapped: Manifest = manifests[0].clone();
    unmapped.capture_id = "cap-tampered-unmapped".to_string();
    for obj in &mut unmapped.objects {
        obj.access = Vec::new();
    }
    let unmapped_path = temp
        .path()
        .join("archive/manifests/99999999-cap-tampered-unmapped.json");
    std::fs::write(&unmapped_path, serde_json::to_string_pretty(&unmapped).unwrap()).unwrap();
    let err = archive.reader("alpha").read_object(&object_sha).unwrap_err();
    assert!(matches!(err, ArchiveError::AccessDenied(_)), "got {err:?}");

    // Orphan object (on disk, never referenced) denies every domain.
    let (orphan_sha, _, _) = archive.put_object(b"orphan bytes").unwrap();
    let err = archive.reader("alpha").read_object(&orphan_sha).unwrap_err();
    assert!(matches!(err, ArchiveError::AccessDenied(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Test 8: status/reconcile track discovered/captured/skipped; a failed capture
// surfaces as an explicit failure state, never as an empty success.
// ---------------------------------------------------------------------------
#[test]
fn status_tracks_counters_and_failures_are_explicit() {
    let (temp, archive) = init_root();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[]);

    // Failed capture (source does not exist): recorded as failed.
    let missing = temp.path().join("nope.jsonl");
    let r = capture_source(&archive, &missing, "codex", DOMAIN, None).unwrap();
    assert!(!r.ok);
    assert_eq!(r.status, "failed");
    let state = archive.load_sources().unwrap();
    let failed_entry = state.sources.values().find(|s| s.status == "failed").unwrap();
    assert!(failed_entry.last_error.is_some());
    assert_eq!(failed_entry.captured, 0);
    assert_eq!(archive.manifests().unwrap().len(), 0, "no fake empty history");

    // Successful capture: counters reflect discovered == captured.
    let r = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(r.records_discovered, 3);
    let state = archive.load_sources().unwrap();
    let entry = state
        .sources
        .values()
        .find(|s| s.status == "captured")
        .unwrap();
    assert_eq!(entry.discovered, 3);
    assert_eq!(entry.captured, 3);
    assert_eq!(entry.skipped, 0);
    assert!(entry.watermark.is_some());

    // Repeat capture: records counted as skipped (dedupe), not re-captured.
    let extra = "{\"timestamp\":\"2026-03-06T21:50:15Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"again\"}]}}".to_string();
    let src = codex_jsonl(temp.path(), "rollout-s-p1.jsonl", &[extra]);
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    let state = archive.load_sources().unwrap();
    let entry = state
        .sources
        .values()
        .find(|s| s.status == "captured")
        .unwrap();
    assert_eq!(entry.discovered, 4);
    assert_eq!(entry.captured, 4);
    assert_eq!(entry.skipped, 3);
}

// ---------------------------------------------------------------------------
// CLI contract smoke: JSON on stdout, stable exit codes (incl. lock refusal).
// ---------------------------------------------------------------------------
#[test]
fn cli_json_contract_and_exit_codes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");

    let out = cli().args(["init", "--root"]).arg(&root).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);

    let src = codex_jsonl(temp.path(), "rollout-cli.jsonl", &[]);
    let out = cli()
        .args(["capture", "--root"])
        .arg(&root)
        .args(["--source"])
        .arg(&src)
        .args(["--provider", "codex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "captured");
    let sha = v["sha256"].as_str().unwrap().to_string();

    let out = cli().args(["status", "--root"]).arg(&root).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["totals"]["manifests"], 1);
    assert_eq!(v["lockHeld"], false);

    // readback via --out, byte-exact
    let out_file = temp.path().join("readback.bin");
    let out = cli()
        .args(["read", "--root"])
        .arg(&root)
        .args(["--sha256", &sha, "--domain", "default", "--out"])
        .arg(&out_file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(std::fs::read(&out_file).unwrap(), std::fs::read(&src).unwrap());

    // wrong domain -> exit 5
    let out = cli()
        .args(["read", "--root"])
        .arg(&root)
        .args(["--sha256", &sha, "--domain", "other"])
        .arg("--out")
        .arg(&out_file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));

    // lock held -> exit 4
    std::fs::write(root.join("locks").join("capture.lock"), b"held").unwrap();
    let out = cli()
        .args(["capture", "--root"])
        .arg(&root)
        .args(["--source"])
        .arg(&src)
        .args(["--provider", "codex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    std::fs::remove_file(root.join("locks").join("capture.lock")).unwrap();

    // capture of a missing source -> exit 7 and explicit failure state
    let out = cli()
        .args(["capture", "--root"])
        .arg(&root)
        .args(["--source"])
        .arg(temp.path().join("missing.jsonl"))
        .args(["--provider", "codex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));

    // usage error -> exit 2
    let out = cli().args(["bogus"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
