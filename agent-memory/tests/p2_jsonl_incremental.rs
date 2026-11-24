//! P2 acceptance tests: JSONL active-session incremental capture.
//!
//! Every source in this file is synthetic and lives under a `tempfile` root; no
//! test points at a real Claude/Codex/OpenCode session directory.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use agent_memory::archive::{hex_sha256, Archive, ArchiveError};
use agent_memory::capture::{capture_source, CaptureReport, Failpoint};
use agent_memory::capture_jsonl::set_post_read_hook;

const DOMAIN: &str = "default";

// ---------------------------------------------------------------------------
// Synthetic Codex-format fixtures (shape only; content is invented)
// ---------------------------------------------------------------------------

fn meta_line(id: &str) -> String {
    serde_json::json!({
        "timestamp": "2026-10-25T10:00:00Z",
        "type": "session_meta",
        "payload": {"id": id, "cwd": "/tmp/synthetic"}
    })
    .to_string()
}

fn rec_line(i: usize, text: &str) -> String {
    serde_json::json!({
        "timestamp": format!("2026-10-25T10:00:{:02}Z", i % 60),
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }
    })
    .to_string()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body).unwrap();
}

fn append_line(path: &Path, line: &str) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
    file.sync_all().unwrap();
}

fn set_mtime(path: &Path, at: SystemTime) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(fs::FileTimes::new().set_modified(at))
        .unwrap();
}

/// Metadata mtime readback (rounded the same way both times, so it is stable to
/// compare even though `FileTimes` may not preserve sub-100 ns precision).
fn mtime_of(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn make_root() -> (tempfile::TempDir, Archive) {
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(temp.path()).unwrap();
    (temp, archive)
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn watermark(archive: &Archive, key: &str) -> agent_memory::archive::Watermark {
    archive
        .load_sources()
        .unwrap()
        .sources
        .get(key)
        .and_then(|s| s.watermark.clone())
        .expect("watermark present")
}

fn capture(archive: &Archive, src: &Path) -> CaptureReport {
    capture_source(archive, src, "codex", DOMAIN, None).unwrap()
}

// ---------------------------------------------------------------------------
// 1. First full capture, duplicate capture, append increment.
// ---------------------------------------------------------------------------
#[test]
fn first_repeat_and_append_are_idempotent() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-a.jsonl");
    let lines = vec![meta_line("sess-a"), rec_line(1, "hello"), rec_line(2, "world")];
    write_lines(&src, &lines);
    let len1 = fs::metadata(&src).unwrap().len();

    let r1 = capture(&archive, &src);
    assert_eq!(r1.status, "captured");
    assert_eq!(r1.change, "initial");
    assert_eq!(r1.generation, 1);
    assert_eq!(r1.records_new, 3);
    assert_eq!(r1.records_deduped, 0);
    assert_eq!(r1.records_discovered, 3);
    assert_eq!(r1.records_malformed, 0);
    assert_eq!(r1.errors, 0);
    assert_eq!(r1.offset, len1);
    assert_eq!(r1.line, 3);
    assert_eq!(r1.file_len, len1);
    assert_eq!(r1.incomplete_tail_bytes, 0);

    // Repeat: cheap-signal skip, no manifest, no object, no record.
    let r2 = capture(&archive, &src);
    assert_eq!(r2.status, "skipped_unchanged");
    assert_eq!(r2.change, "skipped_unchanged");
    assert_eq!(r2.records_new, 0);
    assert_eq!(r2.offset, len1);
    assert_eq!(archive.manifests().unwrap().len(), 1);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);

    // Append one record: one new object (the delta), three already-captured.
    let extra = rec_line(3, "again");
    append_line(&src, &extra);
    let len2 = fs::metadata(&src).unwrap().len();
    let r3 = capture(&archive, &src);
    assert_eq!(r3.status, "captured");
    assert_eq!(r3.change, "append");
    assert_eq!(r3.generation, 1, "an append never bumps the generation");
    assert_eq!(r3.records_new, 1);
    assert_eq!(r3.records_deduped, 3);
    assert_eq!(r3.records_discovered, 4);
    assert_eq!(r3.objects_new, 1);
    assert_eq!(r3.offset, len2);
    assert_eq!(r3.line, 4);
    assert_eq!(archive.manifests().unwrap().len(), 2);
    assert_eq!(archive.load_records().unwrap().records.len(), 4);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 2);

    // Every source line is readable byte-exact.
    let handle = archive.reader(DOMAIN);
    for line in lines.iter().chain(std::iter::once(&extra)) {
        let back = handle.read_record(&hex_sha256(line.as_bytes())).unwrap();
        assert_eq!(back, line.as_bytes());
    }

    // Source locators point into the physical file, not into the chunk.
    let manifests = archive.manifests().unwrap();
    let appended = &manifests[1];
    assert_eq!(appended.source.capture_reason, "append");
    assert_eq!(appended.source.range_start, len1);
    assert_eq!(appended.source.generation, 1);
    assert_eq!(appended.records.len(), 1);
    assert_eq!(appended.records[0].source_offset, Some(len1));
    assert_eq!(appended.records[0].source_line, Some(4));
    assert_eq!(appended.records[0].seq, 3, "seq continues across increments");

    // A further repeat is still a no-op (no duplicate messages or objects).
    let r4 = capture(&archive, &src);
    assert_eq!(r4.status, "skipped_unchanged");
    assert_eq!(archive.manifests().unwrap().len(), 2);
    assert_eq!(archive.load_records().unwrap().records.len(), 4);

    // Several successive increments: exactly one new record + one new delta
    // object each time, never a duplicate.
    for i in 4..7 {
        append_line(&src, &rec_line(i, &format!("more-{i}")));
        let r = capture(&archive, &src);
        assert_eq!(r.change, "append");
        assert_eq!(r.records_new, 1);
        assert_eq!(r.objects_new, 1);
        assert_eq!(r.line, i as u64 + 1);
        assert_eq!(r.offset, fs::metadata(&src).unwrap().len());
    }
    assert_eq!(archive.load_records().unwrap().records.len(), 7);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 5);
    // Repeating the whole pass changes nothing at all.
    let last = capture(&archive, &src);
    assert_eq!(last.status, "skipped_unchanged");
    assert_eq!(archive.manifests().unwrap().len(), 5);
    assert_eq!(archive.load_records().unwrap().records.len(), 7);
}

// ---------------------------------------------------------------------------
// 2. Incomplete trailing record: held back, then completed next round.
// ---------------------------------------------------------------------------
#[test]
fn incomplete_tail_is_held_back_then_completed() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-tail.jsonl");
    let l0 = meta_line("sess-tail");
    let l1 = rec_line(1, "one");
    let l2 = rec_line(2, "two");
    let partial = &l2[..l2.len() / 2];
    let complete_end = (l0.len() + 1 + l1.len() + 1) as u64;
    fs::write(&src, format!("{l0}\n{l1}\n{partial}")).unwrap();
    let file_len = fs::metadata(&src).unwrap().len();

    let r1 = capture(&archive, &src);
    assert_eq!(r1.status, "captured");
    assert_eq!(r1.records_new, 2);
    assert_eq!(r1.offset, complete_end, "watermark stops at the last complete line");
    assert_eq!(r1.file_len, file_len);
    assert_eq!(r1.incomplete_tail_bytes, file_len - complete_end);
    assert_eq!(r1.line, 2);
    // The partial bytes are not a record.
    assert!(!archive
        .load_records()
        .unwrap()
        .records
        .contains_key(&hex_sha256(partial.as_bytes())));

    // Unchanged source: the tail is still held back, nothing is committed.
    let r2 = capture(&archive, &src);
    assert_eq!(r2.status, "skipped_unchanged");
    assert_eq!(r2.records_new, 0);
    assert_eq!(r2.incomplete_tail_bytes, file_len - complete_end);
    assert_eq!(archive.manifests().unwrap().len(), 1);

    // The tail grows but is still unterminated: still nothing committable, yet
    // the coverage row must report the length actually observed now (not the
    // length of the last commit).
    let key = r1.source.clone();
    let more = &l2[l2.len() / 2..l2.len() - 2];
    let mut file = fs::OpenOptions::new().append(true).open(&src).unwrap();
    file.write_all(more.as_bytes()).unwrap();
    drop(file);
    let grown_len = fs::metadata(&src).unwrap().len();
    let r2b = capture(&archive, &src);
    assert_eq!(r2b.status, "skipped_unchanged");
    assert_eq!(r2b.records_new, 0);
    assert_eq!(r2b.incomplete_tail_bytes, grown_len - complete_end);
    let coverage = archive.coverage().unwrap();
    let row = coverage.sources.iter().find(|s| s.source == key).unwrap();
    assert_eq!(row.observed_len, grown_len, "observed length is current");
    assert_eq!(row.watermark_len, file_len, "commit length is unchanged");
    assert!(coverage.ok);

    // Complete the line: the held-back bytes become a committed record.
    fs::write(&src, format!("{l0}\n{l1}\n{l2}\n")).unwrap();
    let total = fs::metadata(&src).unwrap().len();
    let r3 = capture(&archive, &src);
    assert_eq!(r3.change, "append");
    assert_eq!(r3.records_new, 1);
    assert_eq!(r3.offset, total);
    assert_eq!(r3.incomplete_tail_bytes, 0);
    assert_eq!(r3.line, 3);
    let back = archive
        .reader(DOMAIN)
        .read_record(&hex_sha256(l2.as_bytes()))
        .unwrap();
    assert_eq!(back, l2.as_bytes());
}

// ---------------------------------------------------------------------------
// 3. Crash mid-append never advances the watermark; rerun recovers, and the
//    orphaned chunk object is reported (never deleted) then adopted.
// ---------------------------------------------------------------------------
#[test]
fn crash_on_append_keeps_watermark_and_recovers_without_duplicates() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-crash.jsonl");
    let base = vec![meta_line("sess-crash"), rec_line(1, "a"), rec_line(2, "b")];
    write_lines(&src, &base);
    let r1 = capture(&archive, &src);
    let key = r1.source.clone();
    let wm_before = watermark(&archive, &key);

    append_line(&src, &rec_line(3, "c"));
    let manifests_before = archive.manifests().unwrap().len();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = capture_source(
            &archive,
            &src,
            "codex",
            DOMAIN,
            Some(Failpoint::BeforeManifestCommit),
        );
    }));
    assert!(crashed.is_err(), "failpoint must fire");

    let wm_after_crash = watermark(&archive, &key);
    assert_eq!(wm_after_crash.offset, wm_before.offset, "watermark not advanced");
    assert_eq!(archive.manifests().unwrap().len(), manifests_before);
    let orphans = archive.reconcile(&[]).unwrap().orphan_objects;
    assert_eq!(orphans.len(), 1, "the uncommitted chunk is an orphan: {orphans:?}");
    assert!(!archive.lock_held(), "lock released by unwinding");

    // Rerun: the orphan is adopted (same content address), nothing duplicates.
    let r2 = capture(&archive, &src);
    assert_eq!(r2.change, "append");
    assert_eq!(r2.records_new, 1);
    assert_eq!(r2.objects_new, 0, "the crashed run's object is reused");
    assert_eq!(r2.objects_deduped, 1);
    assert!(archive.reconcile(&[]).unwrap().orphan_objects.is_empty());
    assert_eq!(archive.load_records().unwrap().records.len(), 4);
}

// ---------------------------------------------------------------------------
// 4. Growth with an unchanged mtime is still detected as an append.
// ---------------------------------------------------------------------------
#[test]
fn growth_with_unchanged_mtime_is_detected() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-mtime.jsonl");
    write_lines(&src, &[meta_line("sess-m"), rec_line(1, "one"), rec_line(2, "two")]);
    let past = SystemTime::now() - Duration::from_secs(3600);
    set_mtime(&src, past);
    let mtime_before_append = mtime_of(&src);

    let r1 = capture(&archive, &src);
    assert_eq!(r1.records_new, 3);
    let key = r1.source.clone();
    let mtime_at_capture = watermark(&archive, &key).fingerprint.mtime_ms;

    append_line(&src, &rec_line(3, "three"));
    set_mtime(&src, past);
    assert_eq!(
        mtime_of(&src),
        mtime_before_append,
        "mtime must really be unchanged for this case"
    );

    let r2 = capture(&archive, &src);
    assert_eq!(r2.change, "append");
    assert_eq!(r2.records_new, 1);
    assert_eq!(
        watermark(&archive, &key).fingerprint.mtime_ms,
        mtime_at_capture,
        "the fingerprint mtime did not move; detection came from length/anchors"
    );
}

// ---------------------------------------------------------------------------
// 5. active -> archived move: the old path only changes state, the archived
//    path captures as its own physical source without duplicating records.
// ---------------------------------------------------------------------------
#[test]
fn active_to_archived_move_keeps_state_and_dedupes_content() {
    let (temp, archive) = make_root();
    let active = temp.path().join("active.jsonl");
    let archived_dir = temp.path().join("archived");
    fs::create_dir_all(&archived_dir).unwrap();
    let lines = vec![meta_line("sess-move"), rec_line(1, "x"), rec_line(2, "y")];
    write_lines(&active, &lines);

    let r1 = capture(&archive, &active);
    let active_key = r1.source.clone();
    let active_wm = watermark(&archive, &active_key);

    let archived = archived_dir.join("active.jsonl");
    fs::rename(&active, &archived).unwrap();

    // Old path: only the source state changes; archived evidence still reads.
    let r2 = capture(&archive, &active);
    assert_eq!(r2.status, "source_missing");
    assert!(r2.ok);
    assert_eq!(watermark(&archive, &active_key).offset, active_wm.offset);
    let handle = archive.reader(DOMAIN);
    for line in &lines {
        assert_eq!(
            handle.read_record(&hex_sha256(line.as_bytes())).unwrap(),
            line.as_bytes()
        );
    }

    // New path: a fresh physical source, 100% content dedupe, no new object.
    let r3 = capture(&archive, &archived);
    assert_eq!(r3.status, "captured");
    assert_eq!(r3.change, "initial");
    assert_eq!(r3.records_new, 0);
    assert_eq!(r3.records_deduped, 3);
    assert_eq!(r3.objects_new, 0);
    assert_eq!(r3.objects_deduped, 1);
    let archived_key = r3.source.clone();
    assert_ne!(archived_key, active_key);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);
    assert_eq!(archive.load_records().unwrap().records.len(), 3);

    // Both physical sources keep their own watermark (no filename-based merging).
    let state = archive.load_sources().unwrap();
    assert_eq!(state.sources.len(), 2);
    assert_eq!(state.sources[&active_key].status, "source_missing");
    assert_eq!(state.sources[&archived_key].status, "captured");
}

// ---------------------------------------------------------------------------
// 6. resume / fork: physical segments are located separately, shared prefix
//    records dedupe, divergent tail records are captured per segment.
// ---------------------------------------------------------------------------
#[test]
fn resume_fork_segments_are_captured_per_physical_file() {
    let (temp, archive) = make_root();
    let a = temp.path().join("rollout-parent.jsonl");
    let shared = vec![meta_line("sess-fork"), rec_line(1, "shared"), rec_line(2, "shared-2")];
    write_lines(&a, &shared);
    let ra = capture(&archive, &a);
    assert_eq!(ra.records_new, 3);

    // "Fork": a copy of the parent that diverges (a fresh physical file, not a
    // filename-derived child).
    let b = temp.path().join("rollout-child.jsonl");
    fs::copy(&a, &b).unwrap();
    append_line(&b, &rec_line(3, "child-only"));
    let rb = capture(&archive, &b);
    assert_eq!(rb.change, "initial");
    assert_eq!(rb.records_new, 1, "only the divergent record is new");
    assert_eq!(rb.records_deduped, 3);

    // Both segments stay independently readable and independently watermarked.
    assert_ne!(ra.source, rb.source);
    let state = archive.load_sources().unwrap();
    let wa = state.sources[&ra.source].watermark.clone().unwrap();
    let wb = state.sources[&rb.source].watermark.clone().unwrap();
    assert_eq!(wa.offset as usize, fs::metadata(&a).unwrap().len() as usize);
    assert_eq!(wb.offset as usize, fs::metadata(&b).unwrap().len() as usize);
    assert!(wa.offset < wb.offset);
    let handle = archive.reader(DOMAIN);
    assert_eq!(
        handle
            .read_record(&hex_sha256(rec_line(3, "child-only").as_bytes()))
            .unwrap(),
        rec_line(3, "child-only").as_bytes()
    );
    // The fork did not rewrite the parent's capture history.
    assert_eq!(archive.source_captures(&ra.source).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// 7. Non-append changes: truncation, same-length rewrite, rotation / same path
//    different content. Old offsets are never resumed; old objects survive.
// ---------------------------------------------------------------------------
#[test]
fn discontinuity_forces_recapture_and_keeps_old_versions() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-rotate.jsonl");

    let v1 = vec![
        meta_line("sess-v1"),
        rec_line(1, "alpha"),
        rec_line(2, "bravo"),
        rec_line(3, "charlie"),
    ];
    write_lines(&src, &v1);
    let r1 = capture(&archive, &src);
    let key = r1.source.clone();
    assert_eq!(r1.change, "initial");
    assert_eq!(r1.generation, 1);
    assert_eq!(r1.records_new, 4);
    let old_shas: Vec<String> = v1.iter().map(|l| hex_sha256(l.as_bytes())).collect();

    // (a) truncation / length regression
    let v2 = vec![rec_line(10, "delta"), rec_line(11, "echo")];
    write_lines(&src, &v2);
    let r2 = capture(&archive, &src);
    assert_eq!(r2.change, "forced_recapture");
    assert_eq!(r2.change_reason.as_deref(), Some("length_regression"));
    assert_eq!(r2.generation, 2);
    assert_eq!(r2.records_new, 2, "the truncated file is re-read from zero");
    assert_eq!(r2.offset, fs::metadata(&src).unwrap().len());
    assert!(r2.anomalies.iter().any(|a| a.contains("length_regression")));

    // (b) same length, different content (in-place rewrite)
    let v2_len = fs::metadata(&src).unwrap().len();
    let mut v3 = vec![rec_line(20, "f"), rec_line(21, "g")];
    let base_len: usize = v3.iter().map(|l| l.len() + 1).sum();
    let shortfall = v2_len as i64 - base_len as i64;
    assert!(shortfall > 0, "fixture must be shorter than v2");
    v3[1] = rec_line(21, &format!("g{}", "z".repeat(shortfall as usize)));
    write_lines(&src, &v3);
    assert_eq!(
        fs::metadata(&src).unwrap().len(),
        v2_len,
        "same length on purpose"
    );
    let r3 = capture(&archive, &src);
    assert_eq!(r3.change, "forced_recapture");
    assert_eq!(r3.change_reason.as_deref(), Some("same_length_rewrite"));
    assert_eq!(r3.generation, 3);
    assert_eq!(r3.records_new, 2);

    // (c) rotation: a different, longer file at the same path
    let v4: Vec<String> = vec![
        meta_line("sess-v4"),
        rec_line(30, "hotel"),
        rec_line(31, "india"),
        rec_line(32, "juliett"),
    ];
    write_lines(&src, &v4);
    let r4 = capture(&archive, &src);
    assert_eq!(r4.change, "forced_recapture");
    assert_eq!(r4.change_reason.as_deref(), Some("prefix_rewritten"));
    assert_eq!(r4.generation, 4);
    assert_eq!(r4.records_new, 4);
    assert_eq!(r4.offset, fs::metadata(&src).unwrap().len());

    // No stale-offset resume: every current line was captured.
    let index = archive.load_records().unwrap();
    for line in &v4 {
        assert!(
            index.records.contains_key(&hex_sha256(line.as_bytes())),
            "current line missing from the record index"
        );
    }

    // Old versions are retained and still readable (never overwritten).
    let handle = archive.reader(DOMAIN);
    for line in &v1 {
        assert_eq!(
            handle.read_record(&hex_sha256(line.as_bytes())).unwrap(),
            line.as_bytes()
        );
    }
    assert!(old_shas.iter().all(|s| index.records.contains_key(s)));

    // Generation history comes from the manifests.
    let captures = archive.source_captures(&key).unwrap();
    assert_eq!(captures.len(), 4);
    assert_eq!(
        captures.iter().map(|c| c.generation).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        captures.iter().map(|c| c.capture_reason.as_str()).collect::<Vec<_>>(),
        vec!["initial", "forced_recapture", "forced_recapture", "forced_recapture"]
    );

    // Every generation's object is still on disk and referenced.
    let reconcile = archive.reconcile(&[]).unwrap();
    assert!(reconcile.orphan_objects.is_empty(), "{reconcile:?}");
    assert!(reconcile.missing_objects.is_empty(), "{reconcile:?}");
    assert_eq!(reconcile.objects_on_disk, 4);
    assert_eq!(watermark(&archive, &key).generation, 4);
}

// ---------------------------------------------------------------------------
// 8. Interrupted runs: a crash after the manifest commit but before the state
//    save must not duplicate objects or messages, and must not lose records.
// ---------------------------------------------------------------------------
#[test]
fn crash_after_manifest_before_state_recovers_without_duplicates() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-crash2.jsonl");
    write_lines(&src, &[meta_line("sess-c2"), rec_line(1, "one")]);
    let r1 = capture(&archive, &src);
    let key = r1.source.clone();
    let wm1 = watermark(&archive, &key);

    append_line(&src, &rec_line(2, "two"));
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = capture_source(
            &archive,
            &src,
            "codex",
            DOMAIN,
            Some(Failpoint::AfterManifestBeforeState),
        );
    }));
    assert!(crashed.is_err());
    assert_eq!(
        watermark(&archive, &key).offset,
        wm1.offset,
        "state was not saved by the crashed run"
    );

    // Rerun: same content address, no duplicate object, no duplicate record.
    let r2 = capture(&archive, &src);
    assert_eq!(r2.change, "append");
    assert_eq!(r2.records_new, 1);
    assert_eq!(r2.objects_new, 0);
    assert_eq!(
        archive.reconcile(&[]).unwrap().objects_on_disk,
        2,
        "one object per distinct byte range, not per attempt"
    );
    assert_eq!(archive.load_records().unwrap().records.len(), 3);
    let rec = archive.reconcile(&[]).unwrap();
    assert!(rec.orphan_objects.is_empty(), "{rec:?}");
    assert!(rec.missing_objects.is_empty(), "{rec:?}");
}

// ---------------------------------------------------------------------------
// 9. Undecidable concurrent change: explicit degradation, no watermark move.
// ---------------------------------------------------------------------------
fn rewrite_during_read(path: &Path) {
    if path.file_name().and_then(|n| n.to_str()) != Some("rollout-race.jsonl") {
        return;
    }
    fs::write(path, "{\"rotated\":true}\n").unwrap();
}

#[test]
fn concurrent_rewrite_degrades_explicitly_without_advancing_the_watermark() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-race.jsonl");
    write_lines(&src, &[meta_line("sess-race"), rec_line(1, "one")]);
    let r1 = capture(&archive, &src);
    let key = r1.source.clone();
    let wm1 = watermark(&archive, &key);
    let manifests_before = archive.manifests().unwrap().len();

    append_line(&src, &rec_line(2, "two"));
    set_post_read_hook(Some(rewrite_during_read));
    let r2 = capture(&archive, &src);
    set_post_read_hook(None);

    assert_eq!(r2.status, "degraded");
    assert!(!r2.ok, "a degraded run is not a success");
    assert_eq!(r2.change, "degraded");
    assert_eq!(r2.errors, 1);
    assert!(
        r2.anomalies.iter().any(|a| a.starts_with("degraded:")),
        "anomaly must name the degradation: {:?}",
        r2.anomalies
    );
    assert_eq!(watermark(&archive, &key).offset, wm1.offset);
    assert_eq!(archive.manifests().unwrap().len(), manifests_before);

    // The state projection carries the degradation, not a silent success.
    let state = archive.load_sources().unwrap();
    assert_eq!(state.sources[&key].status, "degraded");
    assert!(state.sources[&key].errors >= 1);

    // Coverage surfaces it as not-ok.
    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert!(coverage.sources.iter().any(|s| s.status == "degraded"));

    // Next run classifies the rewritten file as a discontinuity and catches up.
    let r3 = capture(&archive, &src);
    assert_eq!(r3.change, "forced_recapture");
    assert!(r3.generation >= 2);
}

// ---------------------------------------------------------------------------
// 10. Source deletion is state only; readback stays complete. A second
//     concurrent instance is refused on the incremental path too.
// ---------------------------------------------------------------------------
#[test]
fn source_deletion_is_state_only_and_second_instance_is_refused() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-del.jsonl");
    let lines = vec![meta_line("sess-del"), rec_line(1, "keep"), rec_line(2, "me")];
    write_lines(&src, &lines);
    let r1 = capture(&archive, &src);
    let key = r1.source.clone();

    // Second instance refused.
    let guard = archive.acquire_lock().unwrap();
    let err = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap_err();
    assert!(matches!(err, ArchiveError::LockHeld(_)), "got {err:?}");
    drop(guard);

    let object_sha = r1.sha256.clone().unwrap();
    let expected_bytes = {
        let mut v = Vec::new();
        for line in &lines {
            v.extend_from_slice(line.as_bytes());
            v.push(b'\n');
        }
        v
    };
    fs::remove_file(&src).unwrap();
    let r2 = capture(&archive, &src);
    assert_eq!(r2.status, "source_missing");
    assert!(r2.ok);
    assert!(r2.errors == 0);

    let state = archive.load_sources().unwrap();
    assert_eq!(state.sources[&key].status, "source_missing");
    assert!(state.sources[&key].watermark.is_some(), "watermark is kept as evidence");
    assert!(state.sources[&key].last_error.is_none());

    let handle = archive.reader(DOMAIN);
    assert_eq!(handle.read_object(&object_sha).unwrap(), expected_bytes);
    for line in &lines {
        assert_eq!(
            handle.read_record(&hex_sha256(line.as_bytes())).unwrap(),
            line.as_bytes()
        );
    }
}

// ---------------------------------------------------------------------------
// 11. The collector is read-only against the source.
// ---------------------------------------------------------------------------
#[test]
fn capture_never_modifies_the_source() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-ro.jsonl");
    write_lines(&src, &[meta_line("sess-ro"), rec_line(1, "readonly")]);

    let before_bytes = fs::read(&src).unwrap();
    let before_meta = fs::metadata(&src).unwrap();
    let before_mtime = before_meta.modified().unwrap();

    // A read-only attribute makes any write attempt fail at the OS level, so a
    // successful capture proves the collector never opens the source for write.
    let mut perms = before_meta.permissions();
    perms.set_readonly(true);
    fs::set_permissions(&src, perms).unwrap();

    let r = capture(&archive, &src);
    assert_eq!(r.status, "captured");

    let after_meta = fs::metadata(&src).unwrap();
    assert_eq!(fs::read(&src).unwrap(), before_bytes);
    assert_eq!(after_meta.len(), before_meta.len());
    assert_eq!(after_meta.modified().unwrap(), before_mtime);

    let mut perms = after_meta.permissions();
    perms.set_readonly(false);
    fs::set_permissions(&src, perms).unwrap();
}

// ---------------------------------------------------------------------------
// 12. Malformed records are captured (never silently skipped) and counted.
// ---------------------------------------------------------------------------
#[test]
fn malformed_records_are_captured_and_counted() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-bad.jsonl");
    let good = meta_line("sess-bad");
    let broken = "{\"type\":\"response_item\",\"payload\":{"; // invalid JSON, one line
    let tail = rec_line(1, "after");
    fs::write(&src, format!("{good}\n{broken}\n{tail}\n")).unwrap();

    let r = capture(&archive, &src);
    assert_eq!(r.records_new, 3, "raw bytes are evidence even when unparsable");
    assert_eq!(r.records_malformed, 1);
    assert_eq!(r.errors, 1);
    assert!(r.anomalies.iter().any(|a| a.contains("malformed_json")));
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_record(&hex_sha256(broken.as_bytes()))
            .unwrap(),
        broken.as_bytes()
    );

    let state = archive.load_sources().unwrap();
    assert_eq!(state.sources[&r.source].errors, 1);
    assert_eq!(archive.coverage().unwrap().totals.errors_total, 1);

    // Deleting the source later must not replay this run's anomaly as current
    // health (a missing source observed nothing).
    let key = r.source.clone();
    fs::remove_file(&src).unwrap();
    let r2 = capture(&archive, &src);
    assert_eq!(r2.status, "source_missing");
    assert_eq!(r2.change, "source_missing");
    assert!(r2.anomalies.is_empty());
    let coverage = archive.coverage().unwrap();
    assert!(
        coverage.ok,
        "a deleted source with no current anomaly is not an unhealthy state: {coverage:?}"
    );
    let row = coverage.sources.iter().find(|s| s.source == key).unwrap();
    assert!(row.anomalies.is_empty());
    assert_eq!(row.change, "source_missing");
    assert_eq!(row.errors_total, 1, "lifetime counter is kept");
    assert_eq!(row.observed_len, 0, "a missing source observed no bytes");
    assert_eq!(row.incomplete_tail_bytes, 0, "no tail for a missing source");
    assert!(row.watermark_len > 0, "the last commit length is kept as evidence");
}

// ---------------------------------------------------------------------------
// 13. Empty / not-yet-complete first record: tracked without a fake record.
// ---------------------------------------------------------------------------
#[test]
fn empty_and_partial_first_line_are_tracked_without_records() {
    let (temp, archive) = make_root();

    // Empty file: a watermark with no object and no record.
    let empty = temp.path().join("rollout-empty.jsonl");
    fs::write(&empty, "").unwrap();
    let r1 = capture(&archive, &empty);
    assert_eq!(r1.status, "captured");
    assert_eq!(r1.records_new, 0);
    assert_eq!(r1.objects_new, 0);
    assert_eq!(r1.offset, 0);
    assert!(r1.sha256.is_none(), "no object for zero committed bytes");
    let key = r1.source.clone();
    let wm = watermark(&archive, &key);
    assert_eq!((wm.offset, wm.line, wm.records, wm.generation), (0, 0, 0, 1));

    let line = meta_line("sess-empty");
    fs::write(&empty, format!("{line}\n")).unwrap();
    let r2 = capture(&archive, &empty);
    assert_eq!(r2.change, "append");
    assert_eq!(r2.records_new, 1);
    assert_eq!(r2.offset, fs::metadata(&empty).unwrap().len());

    // A brand-new source holding only an incomplete line: tracked, zero records.
    let partial = temp.path().join("rollout-first-partial.jsonl");
    let full = rec_line(1, "eventually");
    let half = &full[..full.len() / 2];
    fs::write(&partial, half).unwrap();
    let r3 = capture(&archive, &partial);
    assert_eq!(r3.status, "captured");
    assert_eq!(r3.records_new, 0);
    assert_eq!(r3.offset, 0);
    assert_eq!(r3.incomplete_tail_bytes, half.len() as u64);
    let pkey = r3.source.clone();
    assert!(watermark(&archive, &pkey).records == 0);

    // Complete it: exactly one record, no duplicate of the partial bytes.
    fs::write(&partial, format!("{full}\n")).unwrap();
    let r4 = capture(&archive, &partial);
    assert_eq!(r4.change, "append");
    assert_eq!(r4.records_new, 1);
    assert_eq!(r4.incomplete_tail_bytes, 0);
    assert!(archive
        .load_records()
        .unwrap()
        .records
        .contains_key(&hex_sha256(full.as_bytes())));
}

// ---------------------------------------------------------------------------
// 14. A repeated *full* capture (watermark lost) still dedupes objects and
//     records instead of duplicating them.
// ---------------------------------------------------------------------------
#[test]
fn repeated_full_capture_without_a_watermark_does_not_duplicate() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-replay.jsonl");
    write_lines(&src, &[meta_line("sess-replay"), rec_line(1, "one"), rec_line(2, "two")]);
    let r1 = capture(&archive, &src);
    assert_eq!(r1.records_new, 3);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);

    // Lose the watermark (state file removed) -> the next run is a full capture.
    fs::remove_file(temp.path().join("state/sources.json")).unwrap();
    let r2 = capture(&archive, &src);
    assert_eq!(r2.change, "initial");
    assert_eq!(r2.records_new, 0, "every record already exists");
    assert_eq!(r2.records_deduped, 3);
    assert_eq!(r2.objects_new, 0);
    assert_eq!(r2.objects_deduped, 1);
    assert_eq!(archive.load_records().unwrap().records.len(), 3);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);
}

// ---------------------------------------------------------------------------
// 15. A P1-era watermark (no P2 probes) never resumes on old offsets: it is
//     forced into a recapture, and nothing is lost.
// ---------------------------------------------------------------------------
#[test]
fn legacy_watermark_without_probes_is_forced_to_recapture() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-legacy.jsonl");
    let lines = vec![meta_line("sess-legacy"), rec_line(1, "one")];
    write_lines(&src, &lines);
    let r1 = capture(&archive, &src);
    let key = r1.source.clone();

    // Downgrade the stored watermark to the P1 shape (no probes, no offset).
    let path = temp.path().join("state").join("sources.json");
    let raw = fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let wm = value["sources"][&key]["watermark"].as_object_mut().unwrap();
    for field in [
        "offset",
        "line",
        "records",
        "generation",
        "head_probe",
        "anchor_probe",
        "probe_len",
    ] {
        wm.remove(field);
    }
    fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    // Appending must not be resumed on the unverifiable offset.
    append_line(&src, &rec_line(2, "two"));
    let r2 = capture(&archive, &src);
    assert_eq!(r2.change, "forced_recapture");
    assert_eq!(
        r2.change_reason.as_deref(),
        Some("legacy_watermark_without_probes")
    );
    assert_eq!(r2.generation, 2);
    assert_eq!(r2.records_deduped, 2, "existing records dedupe");
    assert_eq!(r2.records_new, 1);
    let handle = archive.reader(DOMAIN);
    for line in &lines {
        assert_eq!(
            handle.read_record(&hex_sha256(line.as_bytes())).unwrap(),
            line.as_bytes()
        );
    }
    assert_eq!(
        watermark(&archive, &key).offset,
        fs::metadata(&src).unwrap().len()
    );
}

// ---------------------------------------------------------------------------
// 16. CLI: coverage JSON contract, exit codes unchanged.
// ---------------------------------------------------------------------------
#[test]
fn cli_coverage_reports_counts_and_anomalies() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    let out = cli().args(["init", "--root"]).arg(&root).output().unwrap();
    assert_eq!(out.status.code(), Some(0));

    let src = temp.path().join("rollout-cli2.jsonl");
    write_lines(&src, &[meta_line("sess-cli"), rec_line(1, "one")]);
    let out = cli()
        .args(["capture", "--root"])
        .arg(&root)
        .arg("--source")
        .arg(&src)
        .args(["--provider", "codex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "captured");
    assert_eq!(v["change"], "initial");
    assert_eq!(v["recordsNew"], 2);
    assert_eq!(v["offset"], v["fileLen"]);

    let out = cli().args(["coverage", "--root"]).arg(&root).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["totals"]["sources"], 1);
    assert_eq!(v["totals"]["recordsCommitted"], 2);
    assert_eq!(v["totals"]["capturedTotal"], 2);
    assert_eq!(v["totals"]["errorsTotal"], 0);
    let source = &v["sources"][0];
    assert_eq!(source["recordsCommitted"], 2);
    assert_eq!(source["discoveredLatest"], 2);
    assert_eq!(source["capturedTotal"], 2);
    assert_eq!(source["skippedTotal"], 0);
    assert_eq!(source["generation"], 1);
    assert_eq!(source["observedLen"], v["sources"][0]["watermarkLen"]);
    assert_eq!(source["captures"].as_array().unwrap().len(), 1);
    assert!(source["anomalies"].as_array().unwrap().is_empty());

    // A missing source on a fresh root is still an explicit failure (exit 7).
    let out = cli()
        .args(["capture", "--root"])
        .arg(&root)
        .arg("--source")
        .arg(temp.path().join("absent.jsonl"))
        .args(["--provider", "codex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["ok"], false);
}

// ---------------------------------------------------------------------------
// 17. A truncation whose new content has no terminator must still be classified
//     and reported (regression: it used to be swallowed as `skipped_unchanged`).
// ---------------------------------------------------------------------------
#[test]
fn truncation_to_an_unterminated_fragment_is_reported() {
    let (temp, archive) = make_root();
    let src = temp.path().join("rollout-truncate.jsonl");
    let v1 = vec![
        meta_line("sess-trunc"),
        rec_line(1, "alpha"),
        rec_line(2, "bravo"),
    ];
    write_lines(&src, &v1);
    let r1 = capture(&archive, &src);
    assert_eq!(r1.records_new, 3);
    let key = r1.source.clone();

    // Truncate to a fragment of a record, no trailing newline.
    let fragment = "{\"timestamp\":\"2026-10-25T10:00:30Z\",\"type\":\"resp";
    fs::write(&src, fragment).unwrap();
    let fragment_len = fs::metadata(&src).unwrap().len();

    let r2 = capture(&archive, &src);
    assert_eq!(r2.change, "forced_recapture");
    assert_eq!(r2.change_reason.as_deref(), Some("length_regression"));
    assert_eq!(r2.generation, 2);
    assert_eq!(r2.records_new, 0, "no complete record in the fragment");
    assert_eq!(r2.offset, 0);
    assert_eq!(r2.file_len, fragment_len);
    assert_eq!(r2.incomplete_tail_bytes, fragment_len);
    assert!(
        r2.anomalies.iter().any(|a| a.contains("length_regression")),
        "the truncation must be visible: {:?}",
        r2.anomalies
    );

    // It reaches the coverage report instead of looking unchanged.
    let state = archive.load_sources().unwrap();
    assert_eq!(state.sources[&key].change, "forced_recapture");
    assert_eq!(state.sources[&key].observed_len, Some(fragment_len));
    let coverage = archive.coverage().unwrap();
    let row = coverage.sources.iter().find(|s| s.source == key).unwrap();
    assert_eq!(row.change, "forced_recapture");
    assert_eq!(row.generation, 2);
    assert_eq!(row.observed_len, fragment_len);
    assert!(!coverage.ok, "a discontinuity anomaly is current health");

    // Old generation is still readable.
    for line in &v1 {
        assert_eq!(
            archive
                .reader(DOMAIN)
                .read_record(&hex_sha256(line.as_bytes()))
                .unwrap(),
            line.as_bytes()
        );
    }

    // Completing the fragment yields exactly one record, from offset 0.
    fs::write(&src, format!("{fragment}\"\n")).unwrap();
    let r3 = capture(&archive, &src);
    assert_eq!(r3.change, "append");
    assert_eq!(r3.records_new, 1);
    assert_eq!(r3.offset, fs::metadata(&src).unwrap().len());
    assert_eq!(r3.generation, 2);
}
