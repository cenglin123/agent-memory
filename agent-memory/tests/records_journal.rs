//! implementation-006: the record ledger is a canonical file plus an
//! append-only journal of the same ledger.
//!
//! These cases pin the on-disk contract (spec 1–4), the equivalence oracle
//! (spec 6: `N` appends + compaction == the manifest replay, first writer wins),
//! the cost assertion (a regular round's ledger write follows the new records,
//! not the ledger's total size) and the read paths (spec 5).
//!
//! Everything here is synthetic: a temporary runtime root and a temporary
//! Claude Code JSONL fixture. No real session root, archive or config is read.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use agent_memory::archive::{hex_sha256, Archive, ArchiveError};
use agent_memory::capture::capture_source;
use agent_memory::capture_all::{capture_all, CaptureAllOptions};
use agent_memory::recall::config::load_config;
use serde_json::{json, Value};
use support::matrix as m;

const DOMAIN: &str = "default";
const SESSION: &str = "55555555-5555-4555-8555-555555555555";

fn claude_line(uuid: &str, text: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "timestamp": "2026-10-25T13:25:00Z",
        "sessionId": SESSION,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent dir");
    }
    fs::write(path, body).expect("write fixture");
}

/// One archive root plus its JSONL source.
struct Case {
    root: PathBuf,
    source: PathBuf,
}

impl Case {
    fn new(env: &support::TestEnv, name: &str, lines: &[String]) -> Case {
        let root = env.root(&format!("{name}-archive"));
        Archive::init(&root).expect("init archive");
        let source = env
            .root(&format!("{name}-source"))
            .join("proj")
            .join("a.jsonl");
        write_lines(&source, lines);
        Case { root, source }
    }

    fn archive(&self) -> Archive {
        Archive::open(&self.root).expect("open archive")
    }

    /// Capture the current source with a fresh archive instance, so the
    /// returned ledger stats describe exactly this round.
    fn capture_with(&self) -> (Archive, Value) {
        let archive = self.archive();
        let report =
            capture_source(&archive, &self.source, "claude", DOMAIN, None).expect("capture");
        assert!(report.ok, "capture failed: {:?}", report.error);
        (
            archive,
            serde_json::to_value(&report).expect("capture report"),
        )
    }

    fn capture(&self) -> Value {
        self.capture_with().1
    }

    fn canonical_path(&self) -> PathBuf {
        self.root.join("state").join("records.json")
    }

    fn tail_path(&self) -> PathBuf {
        self.root.join("state").join("records.jsonl")
    }

    /// Entries of the canonical file alone (what an old binary sees).
    fn canonical_entries(&self) -> serde_json::Map<String, Value> {
        let raw = fs::read(self.canonical_path()).expect("read canonical");
        let value: Value = serde_json::from_slice(&raw).expect("canonical parses");
        value["records"].as_object().expect("records map").clone()
    }

    /// Record shas of the journal's complete lines, in file order.
    fn tail_shas(&self) -> Vec<String> {
        let raw = match fs::read_to_string(self.tail_path()) {
            Ok(raw) => raw,
            Err(_) => return Vec::new(),
        };
        let mut shas = Vec::new();
        for line in raw.lines() {
            let value: Value = serde_json::from_str(line).expect("journal line parses");
            shas.push(value["sha"].as_str().expect("sha").to_string());
        }
        shas
    }

    /// `count` journal lines that repeat one existing canonical entry
    /// (duplicates the merge ignores), used to grow the journal cheaply.
    fn repeated_journal(&self, sha: &str, count: usize) -> String {
        let entries = self.canonical_entries();
        let entry = entries.get(sha).expect("seeded canonical entry");
        let line = json!({
            "sha": sha,
            "object": entry["object"],
            "offset": entry["offset"],
            "len": entry["len"],
            "capture_id": entry["capture_id"],
        })
        .to_string();
        let mut out = String::new();
        for _ in 0..count {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Spec 1–2: two forms, one ledger; a regular round only appends.
// ---------------------------------------------------------------------------

#[test]
fn a_capture_round_appends_and_leaves_the_canonical_alone() {
    let env = support::TestEnv::new();
    let first_line = claude_line("u1", "the first captured line");
    let case = Case::new(&env, "append", std::slice::from_ref(&first_line));

    let (_, first) = case.capture_with();
    assert_eq!(first["change"], json!("initial"), "{first}");
    assert_eq!(first["recordsNew"], json!(1), "{first}");
    // A fresh root gets its canonical on the first flush (no journal left).
    assert!(case.canonical_path().exists(), "the canonical exists");
    assert!(!case.tail_path().exists(), "nothing to append yet");
    let canonical_before = fs::read(case.canonical_path()).expect("read canonical");

    let second_line = claude_line("u2", "the second captured line");
    write_lines(&case.source, &[first_line.clone(), second_line.clone()]);
    let (archive, second) = case.capture_with();
    assert_eq!(second["change"], json!("append"), "{second}");
    assert_eq!(second["recordsNew"], json!(1), "{second}");

    assert_eq!(
        fs::read(case.canonical_path()).expect("read canonical"),
        canonical_before,
        "a regular round must not rewrite the canonical ledger"
    );
    let sha2 = hex_sha256(second_line.as_bytes());
    assert_eq!(case.tail_shas(), vec![sha2.clone()]);

    let ledger = archive.load_records().expect("merged ledger");
    assert_eq!(ledger.records.len(), 2);
    m::assert_ledger_matches_manifests(&case.root, "append round");

    let stats = archive.ledger_stats();
    assert_eq!(stats.records_appends, 1, "one append: {stats:?}");
    assert_eq!(stats.records_compactions, 0, "no canonical rewrite");
    assert_eq!(stats.records_written_bytes, stats.records_append_bytes);
    assert!(
        stats.records_append_bytes <= 384,
        "one record costs one journal line: {stats:?}"
    );
}

#[test]
fn the_journal_is_merged_first_writer_wins() {
    let env = support::TestEnv::new();
    let first_line = claude_line("u1", "canonical line");
    let second_line = claude_line("u2", "journal line");
    let case = Case::new(&env, "first-writer", std::slice::from_ref(&first_line));
    case.capture();
    write_lines(&case.source, &[first_line.clone(), second_line.clone()]);
    let (_, report) = case.capture_with();
    assert_eq!(report["recordsNew"], json!(1), "{report}");

    let sha1 = hex_sha256(first_line.as_bytes());
    let sha2 = hex_sha256(second_line.as_bytes());
    let canonical = case.canonical_entries();
    assert!(
        canonical.contains_key(&sha1),
        "the first record is canonical"
    );
    assert!(!canonical.contains_key(&sha2));

    // (a) A duplicate line for a canonical sha must not change its mapping.
    let mut extra = json!({
        "sha": sha1,
        "object": "0".repeat(64),
        "offset": 7,
        "len": 7,
        "capture_id": "cap-should-never-win",
    })
    .to_string();
    extra.push('\n');
    // (b) A duplicate line for a journal sha must not change the first line.
    let entries = case.canonical_entries();
    let _ = entries;
    let mut tail = fs::read_to_string(case.tail_path()).expect("journal");
    tail.push_str(&extra);
    fs::write(case.tail_path(), tail).expect("rewrite journal");

    let archive = case.archive();
    let ledger = archive.load_records().expect("merged ledger");
    assert_eq!(ledger.records.len(), 2);
    assert_eq!(
        ledger.records[&sha1].capture_id,
        canonical[&sha1]["capture_id"].as_str().unwrap(),
        "the canonical mapping wins"
    );
    assert_ne!(
        ledger.records[&sha1].object,
        "0".repeat(64),
        "a journal line never rewrites a canonical mapping"
    );
    m::assert_ledger_matches_manifests(&case.root, "duplicate journal line");
    assert!(
        archive.ledger_stats().records_tail_duplicate_lines >= 1,
        "the duplicate is disclosed, not silently counted twice"
    );

    // A duplicate of a journal line: the earlier line wins, and the ledger is
    // still exactly the manifest replay.
    let kept = ledger.records[&sha2].clone();
    let mut duplicate = json!({
        "sha": sha2,
        "object": "1".repeat(64),
        "offset": 9,
        "len": 9,
        "capture_id": "cap-duplicate",
    })
    .to_string();
    duplicate.push('\n');
    let mut tail = fs::read_to_string(case.tail_path()).expect("journal");
    tail.push_str(&duplicate);
    fs::write(case.tail_path(), tail).expect("rewrite journal");
    let ledger = case.archive().load_records().expect("merged ledger");
    assert_eq!(ledger.records[&sha2].capture_id, kept.capture_id);
    assert_eq!(ledger.records[&sha2].object, kept.object);
    m::assert_ledger_matches_manifests(&case.root, "duplicate journal lines");
}

// ---------------------------------------------------------------------------
// Spec 4: the append crash window (interrupted journal line).
// ---------------------------------------------------------------------------

#[test]
fn an_unterminated_last_line_is_ignored_and_repaired() {
    let env = support::TestEnv::new();
    let first_line = claude_line("u1", "survives the crash");
    let second_line = claude_line("u2", "interrupted append");
    let case = Case::new(&env, "half-line", std::slice::from_ref(&first_line));
    case.capture();
    write_lines(&case.source, &[first_line.clone(), second_line.clone()]);
    case.capture();
    assert_eq!(case.tail_shas().len(), 1);

    // Crash: the append wrote half of its one line, and the round never reached
    // the source state (its watermark stays behind).
    let raw = fs::read(case.tail_path()).expect("journal");
    assert!(raw.len() > 16, "fixture journal: {} bytes", raw.len());
    fs::write(case.tail_path(), &raw[..raw.len() / 2]).expect("truncate journal");
    fs::remove_file(case.root.join("state").join("sources.json")).expect("drop the watermark");

    let archive = case.archive();
    let ledger = archive.load_records().expect("merged ledger");
    assert_eq!(
        ledger.records.len(),
        1,
        "the unterminated line is not merged"
    );
    assert_eq!(
        archive.ledger_stats().records_tail_incomplete_lines,
        1,
        "the ignored half line is disclosed"
    );

    // The next round re-captures (no watermark) and appends after repairing the
    // journal, so the ledger converges on the manifest replay.
    let (archive, report) = case.capture_with();
    assert_eq!(report["change"], json!("initial"), "{report}");
    assert_eq!(report["recordsNew"], json!(1), "{report}");
    let ledger = archive.load_records().expect("merged ledger");
    assert_eq!(ledger.records.len(), 2);
    let tail = fs::read_to_string(case.tail_path()).expect("journal");
    assert!(tail.ends_with('\n'), "the half line is gone: {tail:?}");
    assert_eq!(case.tail_shas(), vec![hex_sha256(second_line.as_bytes())]);
    m::assert_ledger_matches_manifests(&case.root, "recovered append crash");
    // A reader that starts after the repair sees a clean journal (the round's own
    // instance counted the half line it read before repairing it).
    let fresh = case.archive();
    assert_eq!(fresh.load_records().expect("ledger").records.len(), 2);
    assert_eq!(
        fresh.ledger_stats().records_tail_incomplete_lines,
        0,
        "the repaired journal reads cleanly"
    );
}

#[test]
fn a_bad_complete_line_fails_closed() {
    let env = support::TestEnv::new();
    let line = claude_line("u1", "the only record");
    let case = Case::new(&env, "bad-line", std::slice::from_ref(&line));
    // The canonical is written by hand: this case only exercises the journal's
    // parser, so it must not depend on a capture round (nor on compaction).
    let sha = hex_sha256(line.as_bytes());
    let object = "0".repeat(64);
    fs::write(
        case.canonical_path(),
        serde_json::to_string_pretty(&json!({
            "schema": "agent-memory/records@1",
            "records": {
                sha.clone(): {
                    "object": object,
                    "offset": 0,
                    "len": 1,
                    "capture_id": "cap-seeded",
                }
            },
        }))
        .expect("canonical json"),
    )
    .expect("write canonical");
    let entry = case.canonical_entries()[&sha].clone();
    let good = json!({
        "sha": sha,
        "object": entry["object"],
        "offset": entry["offset"],
        "len": entry["len"],
        "capture_id": entry["capture_id"],
    })
    .to_string();

    // Invalid JSON, an invalid record sha, and a missing field: a complete line
    // is never silently dropped.
    for broken in [
        "{this is not json}".to_string(),
        json!({
            "sha": "not-a-sha",
            "object": entry["object"],
            "offset": 0,
            "len": 1,
            "capture_id": "cap",
        })
        .to_string(),
        json!({
            "sha": sha,
            "object": entry["object"],
            "offset": 0,
            "len": 1,
        })
        .to_string(),
    ] {
        fs::write(case.tail_path(), format!("{good}\n{broken}\n").as_bytes())
            .expect("write journal");
        let error = case
            .archive()
            .load_records()
            .expect_err("a complete bad line must fail closed");
        assert!(
            matches!(error, ArchiveError::Corrupt(_)),
            "unexpected error: {error:?}"
        );
    }

    // The CLI reports it as corrupt archive state (exit 3), never as "no
    // records".
    fs::write(case.tail_path(), format!("{good}\n{{broken\n").as_bytes()).expect("write journal");
    let root = case.root.to_string_lossy().to_string();
    let run = support::matrix::run_raw(&["status", "--root", &root]);
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(
        run.stderr.contains("corrupt archive state"),
        "stderr: {}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------
// Spec 3: the threshold compacts at the end of the round; the explicit command.
// ---------------------------------------------------------------------------

#[test]
fn the_journal_threshold_compacts_at_the_end_of_the_round() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "first");
    let second = claude_line("u2", "second");
    let case = Case::new(&env, "threshold", &[first.clone(), second.clone()]);
    case.capture();
    assert_eq!(case.canonical_entries().len(), 2);

    // Grow the journal past the threshold with duplicates the merge ignores, so
    // the ledger content stays exactly the manifest replay.
    let sha1 = hex_sha256(first.as_bytes());
    let big = case.repeated_journal(&sha1, 6000);
    assert!(
        big.len() as u64 > agent_memory::archive::RECORDS_TAIL_COMPACT_BYTES,
        "fixture journal: {} bytes",
        big.len()
    );
    fs::write(case.tail_path(), big.as_bytes()).expect("write journal");

    let third = claude_line("u3", "third");
    write_lines(
        &case.source,
        &[first.clone(), second.clone(), third.clone()],
    );
    let (archive, report) = case.capture_with();
    assert_eq!(report["recordsNew"], json!(1), "{report}");

    assert!(
        !case.tail_path().exists(),
        "the journal is removed after the canonical write"
    );
    assert_eq!(case.canonical_entries().len(), 3);
    let stats = archive.ledger_stats();
    assert_eq!(stats.records_appends, 1, "{stats:?}");
    assert_eq!(
        stats.records_compactions, 1,
        "the threshold triggers exactly one compaction: {stats:?}"
    );
    assert!(
        stats.records_compact_bytes > stats.records_append_bytes,
        "the compaction writes the whole ledger, the append only the change: {stats:?}"
    );
    m::assert_ledger_matches_manifests(&case.root, "threshold compaction");
}

#[test]
fn an_idle_journal_is_compacted_without_reaching_the_threshold() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "first");
    let second = claude_line("u2", "second");
    let case = Case::new(&env, "stale-journal", std::slice::from_ref(&first));
    case.capture();
    write_lines(&case.source, &[first.clone(), second.clone()]);
    case.capture();
    assert_eq!(case.tail_shas().len(), 1);
    let tail_bytes = fs::metadata(case.tail_path()).expect("journal").len();
    assert!(
        tail_bytes < agent_memory::archive::RECORDS_TAIL_COMPACT_BYTES,
        "the fixture stays below the size threshold"
    );

    // Backdate the journal past the age bound: the next round that appends
    // compacts it even though it is small.
    m::set_mtime(&case.tail_path(), m::ago(8 * 24 * 60 * 60));
    let third = claude_line("u3", "third");
    write_lines(&case.source, &[first, second, third]);
    let (archive, report) = case.capture_with();
    assert_eq!(report["recordsNew"], json!(1), "{report}");
    assert!(
        !case.tail_path().exists(),
        "an idle journal is compacted by the next round that appends"
    );
    assert_eq!(case.canonical_entries().len(), 3);
    assert_eq!(archive.ledger_stats().records_compactions, 1);
    m::assert_ledger_matches_manifests(&case.root, "stale journal compaction");
}

#[test]
fn the_explicit_compaction_command_writes_the_canonical_and_drops_the_journal() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "first");
    let second = claude_line("u2", "second");
    let case = Case::new(&env, "explicit", &[first.clone(), second.clone()]);
    case.capture();

    let third = claude_line("u3", "third");
    write_lines(&case.source, &[first, second, third]);
    case.capture();
    assert_eq!(case.tail_shas().len(), 1);
    let root = case.root.to_string_lossy().to_string();

    // `--dry-run` writes nothing and reports what it would do.
    let before = fs::read(case.canonical_path()).expect("canonical");
    let dry = m::run(&["compact-records", "--root", &root, "--dry-run"]);
    assert_eq!(dry.code, 0, "{}", dry.stdout);
    assert_eq!(dry.report["performed"], json!(false));
    assert_eq!(dry.report["dryRun"], json!(true));
    assert_eq!(dry.report["recordsBefore"], json!(2));
    assert_eq!(dry.report["recordsAfter"], json!(3));
    assert!(dry.report["tailBytes"].as_u64().unwrap_or(0) > 0);
    assert_eq!(
        fs::read(case.canonical_path()).expect("canonical"),
        before,
        "a dry run writes nothing"
    );
    assert!(case.tail_path().exists());

    let real = m::run(&["compact-records", "--root", &root]);
    assert_eq!(real.code, 0, "{}", real.stdout);
    assert_eq!(real.report["performed"], json!(true));
    assert_eq!(real.report["recordsBefore"], json!(2));
    assert_eq!(real.report["recordsAfter"], json!(3));
    assert_eq!(real.report["tailRemoved"], json!(true));
    assert_eq!(real.report["tailLines"], json!(1));
    assert!(!case.tail_path().exists());
    assert_eq!(case.canonical_entries().len(), 3);
    m::assert_ledger_matches_manifests(&case.root, "explicit compaction");

    // A second run has nothing to do and says so.
    let again = m::run(&["compact-records", "--root", &root]);
    assert_eq!(again.code, 0, "{}", again.stdout);
    assert_eq!(again.report["performed"], json!(false));
    assert_eq!(again.report["skipped"], json!("nothing_to_compact"));
}

// ---------------------------------------------------------------------------
// Spec 4: the compaction crash window, and the write order.
// ---------------------------------------------------------------------------

#[test]
fn a_compaction_crash_after_the_canonical_write_is_safe() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "first");
    let second = claude_line("u2", "second");
    let case = Case::new(&env, "compact-crash", std::slice::from_ref(&first));
    case.capture();
    write_lines(&case.source, &[first.clone(), second]);
    case.capture();
    let journal = fs::read_to_string(case.tail_path()).expect("journal");
    assert_eq!(case.tail_shas().len(), 1);

    // Compaction succeeded: the canonical holds everything, the journal is gone.
    let report = case
        .archive()
        .compact_records(false)
        .expect("compaction runs");
    assert!(report.performed, "{report:?}");
    assert!(report.tail_removed);
    assert_eq!(case.canonical_entries().len(), 2);

    // Crash window: the canonical was written but the journal was not deleted.
    // Restoring the pre-compaction journal reproduces exactly that state, and it
    // must not change the ledger (canonical keys win) nor fail.
    fs::write(case.tail_path(), journal.as_bytes()).expect("restore journal");
    let archive = case.archive();
    let ledger = archive.load_records().expect("merged ledger");
    assert_eq!(ledger.records.len(), 2);
    assert_eq!(
        archive.ledger_stats().records_tail_duplicate_lines,
        1,
        "the leftover journal line is a duplicate of the canonical entry"
    );
    m::assert_ledger_matches_manifests(&case.root, "compaction crash window");

    // The next round still appends only its own change and the ledger stays the
    // manifest replay.
    let third = claude_line("u3", "third");
    let fourth = claude_line("u4", "fourth");
    write_lines(
        &case.source,
        &[
            claude_line("u1", "first"),
            claude_line("u2", "second"),
            third,
            fourth,
        ],
    );
    let (archive, report) = case.capture_with();
    assert_eq!(report["recordsNew"], json!(2), "{report}");
    assert_eq!(archive.load_records().expect("ledger").records.len(), 4);
    m::assert_ledger_matches_manifests(&case.root, "after the crash window");
}

#[test]
fn a_failed_canonical_write_keeps_the_journal() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "first");
    let second = claude_line("u2", "second");
    let case = Case::new(&env, "failed-canonical", std::slice::from_ref(&first));
    case.capture();
    write_lines(&case.source, &[first.clone(), second.clone()]);
    case.capture();
    assert_eq!(case.tail_shas().len(), 1);

    let canonical_before = fs::read(case.canonical_path()).expect("canonical");
    let journal_before = fs::read(case.tail_path()).expect("journal");
    // Make the canonical *write* fail while the read still works. On Windows the
    // rename over an existing read-only file is refused (ERROR_ACCESS_DENIED);
    // on other platforms a rename over a read-only file succeeds, so the
    // mutation this case pins is only observable on Windows.
    if !cfg!(windows) {
        // Nothing to assert on this platform: the write cannot be made to fail
        // without changing how `atomic_write` works.
        return;
    }
    let mut permissions = fs::metadata(case.canonical_path())
        .expect("canonical metadata")
        .permissions();
    let original = permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(case.canonical_path(), permissions).expect("make canonical read-only");

    let result = case.archive().compact_records(false);

    // Restore the original permissions before touching the files again.
    fs::set_permissions(case.canonical_path(), original).expect("restore canonical permissions");

    let error = result.expect_err("the canonical write must fail");
    assert!(
        matches!(error, ArchiveError::Io(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        fs::read(case.tail_path()).expect("journal"),
        journal_before,
        "the journal is removed only after the canonical is on disk"
    );
    assert_eq!(
        fs::read(case.canonical_path()).expect("canonical"),
        canonical_before,
        "the failed write left the previous canonical in place"
    );

    // Nothing was lost: the merged ledger still equals the manifest replay, and a
    // retried compaction now succeeds.
    let archive = case.archive();
    assert_eq!(archive.load_records().expect("ledger").records.len(), 2);
    m::assert_ledger_matches_manifests(&case.root, "failed canonical write");
    let retried = archive.compact_records(false).expect("the retry compacts");
    assert!(retried.performed, "{retried:?}");
    assert!(retried.tail_removed, "{retried:?}");
    m::assert_ledger_matches_manifests(&case.root, "retried compaction");
}

#[test]
fn a_flush_that_finds_an_outside_writer_reloads_instead_of_overwriting() {
    // Two archive instances on one root (the "a second writer moved the ledger
    // between my load and my flush" shape). The flush must reload the merged
    // view and keep the entry the other instance appended, not overwrite it.
    let env = support::TestEnv::new();
    let first = claude_line("u1", "the seeded record");
    let case = Case::new(&env, "outside-writer", std::slice::from_ref(&first));
    case.capture();

    let left = case.archive();
    let right = case.archive();
    let left_sha = "a".repeat(64);
    let right_sha = "b".repeat(64);
    // Stage on the left instance first, so its cached stamp predates the right
    // instance's append.
    assert!(left
        .stage_record_ref(
            &left_sha,
            agent_memory::archive::RecordRef {
                object: "c".repeat(64),
                offset: 0,
                len: 1,
                capture_id: "cap-left".to_string(),
            },
        )
        .expect("stage left"));
    assert!(right
        .stage_record_ref(
            &right_sha,
            agent_memory::archive::RecordRef {
                object: "d".repeat(64),
                offset: 0,
                len: 1,
                capture_id: "cap-right".to_string(),
            },
        )
        .expect("stage right"));
    right.flush_records().expect("right appends");
    agent_memory::cost::begin_recording();
    left.flush_records().expect("left appends");
    let cost = agent_memory::cost::finish_recording().expect("recording");
    assert_eq!(
        cost.counters
            .get("records_tail.stale_reloads")
            .copied()
            .unwrap_or(0),
        1,
        "the left flush reloaded because the stamp moved: {cost:?}"
    );

    let ledger = case.archive().load_records().expect("merged ledger");
    assert!(
        ledger.records.contains_key(&left_sha),
        "the left writer's entry is not lost"
    );
    assert!(
        ledger.records.contains_key(&right_sha),
        "the right writer's entry is not overwritten"
    );
    assert_eq!(ledger.records.len(), 3, "one seeded + two staged records");

    // A line written behind this instance's back must also invalidate the read
    // cache (the journal's stamp is part of the cache key): a stale cached view
    // here would hide records until the process restarted.
    let outside_sha = "e".repeat(64);
    let mut line = json!({
        "sha": outside_sha,
        "object": "f".repeat(64),
        "offset": 0,
        "len": 1,
        "capture_id": "cap-outside",
    })
    .to_string();
    line.push('\n');
    let mut tail = fs::read_to_string(case.tail_path()).expect("journal");
    tail.push_str(&line);
    fs::write(case.tail_path(), tail).expect("append outside line");
    assert!(
        right
            .load_records()
            .expect("ledger")
            .records
            .contains_key(&outside_sha),
        "a journal change invalidates the cached read view"
    );
}

#[test]
fn concurrent_compactions_never_lose_records() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "first");
    let second = claude_line("u2", "second");
    let case = Case::new(&env, "concurrent", std::slice::from_ref(&first));
    case.capture();
    write_lines(&case.source, &[first.clone(), second.clone()]);
    case.capture();
    let third = claude_line("u3", "third");
    write_lines(&case.source, &[first, second, third]);
    case.capture();
    assert_eq!(case.tail_shas().len(), 2);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let root = case.root.clone();
            thread::spawn(move || {
                Archive::open(&root)
                    .expect("archive opens")
                    .compact_records(false)
            })
        })
        .collect();
    let mut performed = 0;
    let mut locked = 0;
    for handle in handles {
        match handle.join().expect("compaction thread") {
            Ok(report) => {
                performed += 1;
                assert_eq!(report.records_after, 3, "{report:?}");
                assert!(!report.tail_kept_because_changed, "{report:?}");
            }
            // The loser of the race reports the held lock; it never interleaves
            // two rewrites and never removes a journal it did not merge.
            Err(ArchiveError::LockHeld(_)) => locked += 1,
            Err(other) => panic!("unexpected compaction error: {other:?}"),
        }
    }
    assert!(performed >= 1, "somebody compacted");
    assert_eq!(performed + locked, 4, "every run is accounted for");

    let archive = case.archive();
    assert_eq!(archive.load_records().expect("ledger").records.len(), 3);
    assert_eq!(case.canonical_entries().len(), 3);
    assert!(
        !case.tail_path().exists(),
        "the winning compaction removed the journal"
    );
    m::assert_ledger_matches_manifests(&case.root, "concurrent compaction");
}

// ---------------------------------------------------------------------------
// Concurrency between the two writers of the same journal: an append and a
// compaction's "the journal is still mine, remove it" step.
// ---------------------------------------------------------------------------

/// Stage one synthetic record (sha = `index`, object = `0x1000 + index`).
/// Manifest-less records keep the mapping this test staged, which is what makes
/// "did the entry survive with its first mapping" checkable.
fn stage_synthetic(archive: &Archive, index: u64) -> Result<bool, ArchiveError> {
    archive.stage_record_ref(
        &format!("{index:064x}"),
        agent_memory::archive::RecordRef {
            object: format!("{:064x}", 0x1000 + index),
            offset: index * 3,
            len: 3,
            capture_id: format!("cap-{index}"),
        },
    )
}

/// Stage one synthetic record and append it (one flush per record).
fn append_synthetic(archive: &Archive, index: u64) -> Result<(), ArchiveError> {
    if stage_synthetic(archive, index)? {
        archive.flush_records()?;
    }
    Ok(())
}

fn journal_lock_path(case: &Case) -> PathBuf {
    case.root.join("locks").join("records-tail.lock")
}

#[test]
fn an_append_that_cannot_lock_the_journal_promotes_instead_of_writing_it() {
    // `locks/records-tail.lock` is what a compaction holds between "the journal
    // is still the one I merged" and its `remove_file`. Holding it here (as a
    // compaction inside that window, or a hard-killed run, does) proves the
    // append neither slips its line into a journal that is about to be deleted
    // nor loses it: the wait is bounded, and past the budget the entry goes into
    // the canonical instead of the journal (an unlocked journal write is the one
    // interleaving that could delete an entry before it was merged).
    let env = support::TestEnv::new();
    let first = claude_line("u1", "seeded record");
    let case = Case::new(&env, "journal-lock", std::slice::from_ref(&first));
    case.capture();
    let archive = case.archive();
    append_synthetic(&archive, 9001).expect("journal line");
    assert_eq!(case.tail_shas().len(), 1);

    fs::write(journal_lock_path(&case), b"pid=0 acquired_at_unix_ms=0\n")
        .expect("hold the journal lock");

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let root = case.root.clone();
    let appender = thread::spawn(move || -> Result<(), ArchiveError> {
        let archive = Archive::open(&root)?;
        started_tx.send(()).ok();
        append_synthetic(&archive, 9002)?;
        done_tx.send(()).ok();
        Ok(())
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the appender started");
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(120))
            .is_err(),
        "the append first waits for the journal lock, it does not write into a \
         journal a compaction is about to remove"
    );
    assert_eq!(
        case.tail_shas().len(),
        1,
        "nothing was appended while the journal lock was held"
    );

    // Bounded wait, then the degraded path: the entry is in the canonical and
    // the journal is untouched, so the record is durable even though a
    // compaction may be about to delete that journal.
    done_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the degraded flush completes within its budget");
    appender.join().expect("appender thread").expect("append");
    assert_eq!(
        case.tail_shas().len(),
        1,
        "the journal is never written without its lock"
    );
    assert_eq!(
        case.canonical_entries().len(),
        3,
        "the promotion writes the merged ledger: the seed, the journal line and          the staged entry"
    );

    let ledger = case.archive().load_records().expect("merged ledger");
    for index in [9001u64, 9002] {
        let sha = format!("{index:064x}");
        let reference = ledger
            .records
            .get(&sha)
            .unwrap_or_else(|| panic!("record {index} survives"));
        assert_eq!(reference.capture_id, format!("cap-{index}"));
    }

    // With the stale file gone the journal is used again.
    fs::remove_file(journal_lock_path(&case)).expect("release the journal lock");
    append_synthetic(&archive, 9003).expect("journal append");
    assert_eq!(case.tail_shas().len(), 2, "the append path is back");
    let ledger = case.archive().load_records().expect("merged ledger");
    assert_eq!(
        ledger.records.len(),
        4,
        "the seed plus three synthetic records"
    );
}

#[test]
fn a_concurrent_append_and_compaction_never_loses_a_committed_ledger_entry() {
    // One thread appends a line per record, another compacts in a loop. Every
    // append that returned `Ok` (a committed entry) must still be in the merge
    // with the mapping its first write gave it.
    let env = support::TestEnv::new();
    let first = claude_line("u1", "seeded record");
    let case = Case::new(&env, "append-vs-compaction", std::slice::from_ref(&first));
    case.capture();

    const APPENDS: u64 = 400;
    const COMPACTIONS: usize = 120;

    let appender_root = case.root.clone();
    let appender = thread::spawn(move || -> Result<(), ArchiveError> {
        let archive = Archive::open(&appender_root)?;
        for index in 0..APPENDS {
            append_synthetic(&archive, index + 1)?;
        }
        Ok(())
    });
    let compactor_root = case.root.clone();
    let compactor = thread::spawn(move || -> Result<(), ArchiveError> {
        let archive = Archive::open(&compactor_root)?;
        for _ in 0..COMPACTIONS {
            match archive.compact_records(false) {
                // A second compaction holding the lock is a normal outcome.
                Ok(_) | Err(ArchiveError::LockHeld(_)) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(())
    });
    appender.join().expect("appender thread").expect("appends");
    compactor
        .join()
        .expect("compactor thread")
        .expect("compactions");

    let ledger = case.archive().load_records().expect("merged ledger");
    for index in 0..APPENDS {
        let sha = format!("{:064x}", index + 1);
        let reference = ledger
            .records
            .get(&sha)
            .unwrap_or_else(|| panic!("journal line {index} was lost"));
        let expected_object = format!("{:064x}", 0x1000 + index + 1);
        let expected_capture = format!("cap-{}", index + 1);
        assert_eq!(
            (
                reference.object.as_str(),
                reference.offset,
                reference.len,
                reference.capture_id.as_str()
            ),
            (
                expected_object.as_str(),
                (index + 1) * 3,
                3,
                expected_capture.as_str()
            ),
            "record {index} keeps its first mapping"
        );
    }
    assert_eq!(ledger.records.len(), APPENDS as usize + 1);
}

#[test]
fn a_reader_never_sees_a_half_compacted_ledger() {
    // Every read must be some state the ledger really had: it may never be
    // missing an entry whose append had already completed when the read started.
    // The seed is large enough that reading the canonical straddles a concurrent
    // rewrite, which is the window that used to mix "canonical before the
    // rewrite" with "journal already gone".
    let env = support::TestEnv::new();
    let first = claude_line("u1", "seeded record");
    let case = Case::new(&env, "reader-vs-compaction", std::slice::from_ref(&first));
    case.capture();
    const SEED: u64 = 2000;
    const APPENDS: u64 = 200;
    {
        let archive = case.archive();
        for index in 0..SEED {
            assert!(
                stage_synthetic(&archive, 10_000 + index).expect("stage seed"),
                "the seed records are new"
            );
        }
        archive
            .flush_records()
            .expect("one journal append for the seed");
        archive.compact_records(false).expect("seed compaction");
    }
    assert_eq!(case.canonical_entries().len(), SEED as usize + 1);

    let completed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let appender_root = case.root.clone();
    let appender_completed = std::sync::Arc::clone(&completed);
    let appender = thread::spawn(move || -> Result<(), ArchiveError> {
        let archive = Archive::open(&appender_root)?;
        for index in 0..APPENDS {
            append_synthetic(&archive, 20_000 + index)?;
            appender_completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    });
    let compactor_root = case.root.clone();
    let appender_done = std::sync::Arc::clone(&completed);
    let compactor = thread::spawn(move || -> Result<u64, ArchiveError> {
        let archive = Archive::open(&compactor_root)?;
        let mut rounds = 0u64;
        while appender_done.load(std::sync::atomic::Ordering::SeqCst) < APPENDS && rounds < 2_000 {
            match archive.compact_records(false) {
                Ok(_) | Err(ArchiveError::LockHeld(_)) => {}
                Err(other) => return Err(other),
            }
            rounds += 1;
        }
        Ok(rounds)
    });

    for _ in 0..40 {
        let appended_before = completed.load(std::sync::atomic::Ordering::SeqCst);
        let ledger = Archive::open(&case.root)
            .expect("reader archive")
            .load_records()
            .expect("merged ledger");
        assert!(
            ledger.records.len() as u64 >= SEED + 1 + appended_before,
            "a read returned a ledger that never existed: {} entries after \
             {appended_before} appends completed",
            ledger.records.len()
        );
    }

    appender.join().expect("appender thread").expect("appends");
    let rounds = compactor
        .join()
        .expect("compactor thread")
        .expect("compactions");
    assert!(rounds >= 1, "the compactor ran");
    let ledger = case.archive().load_records().expect("merged ledger");
    assert_eq!(ledger.records.len() as u64, SEED + 1 + APPENDS);
}

// ---------------------------------------------------------------------------
// Spec 6: equivalence at two scales + the cost assertion.
// ---------------------------------------------------------------------------

fn seeded_ledger(env: &support::TestEnv, name: &str, count: usize) -> Case {
    let lines: Vec<String> = (0..count)
        .map(|index| claude_line(&format!("u{index}"), &format!("seed message {index}")))
        .collect();
    let case = Case::new(env, name, &lines);
    let report = case.capture();
    assert_eq!(report["recordsNew"], json!(count as u64), "{report}");
    case
}

#[test]
fn a_regular_round_writes_only_the_change_whatever_the_ledger_size() {
    let env = support::TestEnv::new();
    let mut append_bytes = Vec::new();
    for (name, count) in [("small-ledger", 40usize), ("large-ledger", 4000usize)] {
        let case = seeded_ledger(&env, name, count);
        let total_before = case.canonical_entries().len();
        assert_eq!(total_before, count, "the canonical holds the whole ledger");

        let extra = claude_line("u-extra", "one new record");
        let mut lines: Vec<String> = (0..count)
            .map(|index| claude_line(&format!("u{index}"), &format!("seed message {index}")))
            .collect();
        lines.push(extra.clone());
        write_lines(&case.source, &lines);
        let (archive, report) = case.capture_with();
        assert_eq!(report["recordsNew"], json!(1), "{report}");

        let stats = archive.ledger_stats();
        assert_eq!(stats.records_appends, 1, "{name}: {stats:?}");
        assert_eq!(stats.records_compactions, 0, "{name}: {stats:?}");
        assert!(
            stats.records_append_bytes <= 384,
            "{name}: one new record must cost one journal line, got {} bytes",
            stats.records_append_bytes
        );
        assert_eq!(
            archive.load_records().expect("ledger").records.len(),
            count + 1
        );
        m::assert_ledger_matches_manifests(&case.root, name);
        append_bytes.push(stats.records_append_bytes);
    }
    assert_eq!(
        append_bytes[0], append_bytes[1],
        "the regular-round ledger write is independent of the ledger's total size"
    );
}

#[test]
fn a_compaction_writes_the_whole_ledger() {
    let env = support::TestEnv::new();
    let mut sizes = Vec::new();
    for (name, count) in [("compact-small", 40usize), ("compact-large", 4000usize)] {
        let case = seeded_ledger(&env, name, count);
        let sha = hex_sha256(claude_line("u0", "seed message 0").as_bytes());
        fs::write(case.tail_path(), case.repeated_journal(&sha, 2).as_bytes())
            .expect("write journal");
        let report = case
            .archive()
            .compact_records(false)
            .expect("compaction runs");
        assert!(report.performed, "{name}: {report:?}");
        assert_eq!(report.records_before, count as u64, "{name}: {report:?}");
        assert_eq!(report.records_after, count as u64, "{name}: {report:?}");
        assert!(report.tail_removed, "{name}: {report:?}");
        assert!(report.bytes_written > 0);
        m::assert_ledger_matches_manifests(&case.root, name);
        sizes.push(report.bytes_written);
    }
    assert!(
        sizes[1] > sizes[0] * 10,
        "compaction cost follows the ledger size: {sizes:?}"
    );
}

// ---------------------------------------------------------------------------
// Spec 5: every read path serves the merged ledger.
// ---------------------------------------------------------------------------

#[test]
fn every_read_path_serves_the_merged_ledger() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "canonical record");
    let second = claude_line("u2", "journal-only record");
    let case = Case::new(&env, "read-paths", std::slice::from_ref(&first));
    case.capture();
    let sha_second = hex_sha256(second.as_bytes());
    // The second round appends one line: that record lives in the journal only.
    write_lines(&case.source, &[first, second.clone()]);
    let (archive, _) = case.capture_with();
    assert!(
        !case.canonical_entries().contains_key(&sha_second),
        "the fixture record lives in the journal only"
    );

    // Library paths.
    assert!(archive.records_contains(&sha_second).expect("contains"));
    let reference = archive.record_ref(&sha_second).expect("reference");
    assert_eq!(reference.len, second.len() as u64);
    let bytes = archive
        .reader(DOMAIN)
        .read_record(&sha_second)
        .expect("record readback");
    assert_eq!(bytes, second.as_bytes());
    assert!(archive
        .load_records()
        .expect("ledger")
        .records
        .contains_key(&sha_second));

    // CLI `read` (record branch) and `status` totals.
    let root = case.root.to_string_lossy().to_string();
    let out = env.path().join("readback.bin");
    let out_text = out.to_string_lossy().to_string();
    let run = m::run(&[
        "read",
        "--root",
        &root,
        "--sha256",
        &sha_second,
        "--domain",
        DOMAIN,
        "--out",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(run.report["kind"], json!("record"), "{}", run.stdout);
    assert_eq!(run.report["verified"], json!(true));
    assert_eq!(fs::read(&out).expect("readback"), second.as_bytes());

    let status = m::run(&["status", "--root", &root]);
    assert_eq!(status.code, 0, "{}", status.stdout);
    assert_eq!(status.report["totals"]["records"], json!(2));
    m::assert_ledger_matches_manifests(&case.root, "read paths");
}

#[test]
fn a_downgrade_without_a_compaction_misses_the_journal() {
    // This is the UPGRADE contract: an older binary reads `records.json` only,
    // so the journal must be compacted before a downgrade.
    let env = support::TestEnv::new();
    let first = claude_line("u1", "canonical record");
    let second = claude_line("u2", "journal-only record");
    let case = Case::new(&env, "downgrade", std::slice::from_ref(&first));
    case.capture();
    write_lines(&case.source, &[first, second]);
    case.capture();

    let old_view = case.canonical_entries();
    assert_eq!(old_view.len(), 1, "an old binary sees one record");
    assert_eq!(
        case.archive().load_records().expect("ledger").records.len(),
        2
    );

    let report = case
        .archive()
        .compact_records(false)
        .expect("compaction runs");
    assert!(report.performed, "{report:?}");
    assert_eq!(report.records_before, 1);
    assert_eq!(report.records_after, 2);
    assert!(!case.tail_path().exists());
    assert_eq!(
        case.canonical_entries().len(),
        2,
        "after compacting, an old binary sees the whole ledger"
    );
    m::assert_ledger_matches_manifests(&case.root, "downgrade-ready");
}

#[test]
fn save_records_replaces_the_whole_ledger_and_drops_the_journal() {
    let env = support::TestEnv::new();
    let first = claude_line("u1", "canonical record");
    let second = claude_line("u2", "journal-only record");
    let case = Case::new(&env, "save-records", std::slice::from_ref(&first));
    case.capture();
    write_lines(&case.source, &[first, second]);
    case.capture();
    assert_eq!(case.tail_shas().len(), 1);

    let archive = case.archive();
    let mut ledger = archive.load_records().expect("ledger");
    let kept: Vec<String> = ledger
        .records
        .keys()
        .filter(|sha| **sha != hex_sha256(claude_line("u2", "journal-only record").as_bytes()))
        .cloned()
        .collect();
    assert_eq!(kept.len(), 1);
    ledger.records.retain(|sha, _| kept.contains(sha));
    archive.save_records(&ledger).expect("replacement");

    assert!(!case.tail_path().exists(), "the journal is dropped");
    assert_eq!(archive.load_records().expect("ledger").records.len(), 1);
    assert_eq!(case.canonical_entries().len(), 1);
    assert_eq!(archive.ledger_stats().records_compactions, 1);
}

// ---------------------------------------------------------------------------
// Spec 5: the capture-all round keeps its 005 semantics.
// ---------------------------------------------------------------------------

fn round(root: &Path, claude_root: &Path, env: &support::TestEnv, name: &str) -> Value {
    let config = env.write_json(
        name,
        &env.config_value(
            vec![env.md_source("claude_code", "claude_code", claude_root)],
            vec!["claude_code"],
        ),
    );
    let (report, code) = capture_all(
        &load_config(&config).expect("config loads"),
        &CaptureAllOptions {
            root: Some(root.to_string_lossy().to_string()),
            domain: DOMAIN.to_string(),
            ..Default::default()
        },
    )
    .expect("capture-all runs");
    assert_eq!(code, 0);
    serde_json::to_value(&report).expect("round report")
}

#[test]
fn the_005_round_semantics_are_unchanged() {
    let env = support::TestEnv::new();
    let root = env.root("round-ledger");
    Archive::init(&root).expect("init runtime root");
    let claude = env.root("round-claude");
    for index in 0..2 {
        write_lines(
            &claude
                .join("proj")
                .join(format!("11111111-1111-4111-8111-00000000000{index}.jsonl")),
            &[claude_line(&format!("u{index}"), &format!("round {index}"))],
        );
    }

    let first = round(&root, &claude, &env, "round-1.json");
    assert_eq!(first["ledgers"]["recordsLoads"], json!(1), "{first}");
    assert_eq!(first["ledgers"]["recordsFlushes"], json!(1), "{first}");
    assert_eq!(first["ledgers"]["sourcesFlushes"], json!(1), "{first}");
    assert_eq!(first["ledgers"]["recordsAppends"], json!(1), "{first}");
    assert_eq!(
        first["ledgers"]["recordsCompactions"],
        json!(1),
        "a fresh root writes its canonical on the first flush: {first}"
    );
    m::assert_ledger_matches_manifests(&root, "round 1");

    // One file gains a record: one append, no canonical rewrite.
    let file = claude
        .join("proj")
        .join("11111111-1111-4111-8111-000000000000.jsonl");
    write_lines(
        &file,
        &[claude_line("u0", "round 0"), claude_line("u9", "new")],
    );
    let second = round(&root, &claude, &env, "round-2.json");
    assert_eq!(second["ledgers"]["recordsLoads"], json!(1), "{second}");
    assert_eq!(second["ledgers"]["recordsFlushes"], json!(1), "{second}");
    assert_eq!(second["ledgers"]["recordsAppends"], json!(1), "{second}");
    assert_eq!(
        second["ledgers"]["recordsCompactions"],
        json!(0),
        "{second}"
    );
    assert!(
        second["ledgers"]["recordsAppendBytes"]
            .as_u64()
            .unwrap_or(0)
            <= 384,
        "{second}"
    );
    m::assert_ledger_matches_manifests(&root, "round 2");

    // A round with nothing new touches no ledger at all.
    let third = round(&root, &claude, &env, "round-3.json");
    assert_eq!(third["totals"]["files_captured"], json!(0), "{third}");
    assert_eq!(third["ledgers"]["recordsLoads"], json!(0), "{third}");
    assert_eq!(third["ledgers"]["recordsFlushes"], json!(0), "{third}");
    assert_eq!(third["ledgers"]["recordsAppends"], json!(0), "{third}");
}
