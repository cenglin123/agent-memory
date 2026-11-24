//! implementation-005 specs 2–4: one read per evidence object, one ledger read
//! and write per capture round, one reconcile per round.
//!
//! These are cost assertions on counters, not timing assertions: they hold on
//! any machine and they are the mechanical evidence that a round no longer reads
//! the same thing N times.

use std::fs;
use std::path::{Path, PathBuf};

use agent_memory::archive::Archive;
use agent_memory::capture::capture_source;
use agent_memory::capture_all::{capture_all, CaptureAllOptions};
use agent_memory::cost;
use agent_memory::recall::config::load_config;
use serde_json::Value;

mod support;

use support::TestEnv;

const DOMAIN: &str = "default";

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent dir");
    }
    fs::write(path, body).expect("write fixture file");
}

fn claude_line(session: &str, text: &str) -> String {
    format!(
        "{{\"type\":\"user\",\"sessionId\":\"{session}\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}"
    )
}

fn claude_root(env: &TestEnv, files: usize) -> PathBuf {
    let root = env.root("claude-projects");
    for index in 0..files {
        write_lines(
            &root
                .join("proj")
                .join(format!("11111111-1111-4111-8111-00000000000{index}.jsonl")),
            &[claude_line(&format!("s{index}"), &format!("needle {index}"))],
        );
    }
    root
}

struct RoundReport {
    value: Value,
    code: u8,
}

fn run_capture_all(root: &Path, claude_root: &Path, env: &TestEnv, name: &str) -> RoundReport {
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
    RoundReport {
        value: serde_json::to_value(&report).expect("report json"),
        code,
    }
}

fn source_counter(report: &Value, source_id: &str, counter: &str) -> u64 {
    report["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .find(|entry| entry["source_id"] == serde_json::json!(source_id))
        .and_then(|entry| entry["cost"]["counters"][counter].as_u64())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 3. records ledger: one read, one write per capture-all round.
// ---------------------------------------------------------------------------

#[test]
fn one_records_read_and_one_write_per_capture_all_round() {
    let env = TestEnv::new();
    let root = env.root("round-ledger");
    Archive::init(&root).expect("init runtime root");
    let claude = claude_root(&env, 3);

    let round = run_capture_all(&root, &claude, &env, "round-config-1.json");
    assert_eq!(round.code, 0, "{}", round.value);
    let report = round.value;
    // Three captured files, one ledger load and one ledger write for the round
    // (the load happens inside the first capturing source, the write after the
    // last one; the round counters are the authoritative totals).
    assert_eq!(
        source_counter(&report, "claude_code", "records_read.calls"),
        1,
        "the record ledger is loaded once per round: {report}"
    );
    assert_eq!(
        report["ledgers"]["recordsFlushes"], 1,
        "the record ledger is written once per round: {report}"
    );
    assert_eq!(
        report["ledgers"]["sourcesFlushes"], 1,
        "the source state is written once per round: {report}"
    );
    assert!(
        report["ledgers"]["recordsReadBytes"].as_u64().unwrap_or(0) > 0
            || report["ledgers"]["recordsLoads"] == 1,
        "the ledger read is accounted: {report}"
    );
    assert_eq!(report["ledgers"]["recordsLoads"], 1, "{report}");
    assert_eq!(report["ledgers"]["recordsFlushes"], 1, "{report}");
    assert_eq!(report["ledgers"]["sourcesFlushes"], 1, "{report}");

    // A round with nothing new captures nothing, so it touches no ledger.
    let quiet = run_capture_all(&root, &claude, &env, "round-config-2.json");
    assert_eq!(quiet.code, 0, "{}", quiet.value);
    let quiet = quiet.value;
    assert_eq!(quiet["totals"]["files_captured"], 0, "{quiet}");
    assert_eq!(
        quiet["ledgers"]["recordsLoads"], 0,
        "an unchanged round reads no ledger: {quiet}"
    );
    assert_eq!(quiet["ledgers"]["recordsFlushes"], 0, "{quiet}");
}

#[test]
fn record_readback_reuses_the_ledger_snapshot() {
    let env = TestEnv::new();
    let root = env.root("round-records-read");
    Archive::init(&root).expect("init runtime root");
    let archive = Archive::open(&root).expect("archive opens");
    let claude = claude_root(&env, 1);
    let file = claude.join("proj").join("11111111-1111-4111-8111-000000000000.jsonl");
    capture_source(&archive, &file, "claude", DOMAIN, None).expect("capture");

    let shas: Vec<String> = archive
        .load_records()
        .expect("ledger")
        .records
        .keys()
        .cloned()
        .collect();
    assert!(!shas.is_empty(), "the capture committed records");

    cost::begin_recording();
    for sha in &shas {
        archive
            .reader(DOMAIN)
            .read_record(sha)
            .expect("record readable");
    }
    // Repeat: the second pass must not reload either.
    for sha in &shas {
        archive
            .reader(DOMAIN)
            .read_record(sha)
            .expect("record readable");
    }
    let cost = cost::finish_recording().expect("recording");
    assert_eq!(
        cost.counters.get("records_read.calls").copied().unwrap_or(0),
        0,
        "read_record reuses the loaded ledger: {cost:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. reconcile: once per capture-all round (never per captured file).
// ---------------------------------------------------------------------------

#[test]
fn one_reconcile_per_capture_all_round() {
    let env = TestEnv::new();
    let root = env.root("round-reconcile");
    Archive::init(&root).expect("init runtime root");
    let claude = claude_root(&env, 3);

    let round = run_capture_all(&root, &claude, &env, "reconcile-config-1.json");
    assert_eq!(round.code, 0, "{}", round.value);
    let report = round.value;
    assert_eq!(
        report["cost"]["counters"]["reconcile.calls"], 1,
        "one object-directory walk per round: {report}"
    );
    assert!(
        report["reconcile"]["objects_on_disk"].as_u64().unwrap_or(0) >= 3,
        "the round-level reconcile reports the store: {report}"
    );
    // The counters the README documents survive the round-level cost sample (they
    // used to be inserted before `Cost::sample` replaced the map).
    for counter in [
        "reconcile.objects_on_disk",
        "reconcile.objects_referenced",
        "reconcile.orphan_objects",
    ] {
        assert!(
            report["cost"]["counters"][counter].is_u64(),
            "the round report carries {counter}: {report}"
        );
    }
    assert_eq!(
        report["cost"]["counters"]["reconcile.objects_on_disk"],
        report["reconcile"]["objects_on_disk"],
        "the counter and the reconcile block are the same number: {report}"
    );
    assert_eq!(
        report["cost"]["counters"]["reconcile.objects_referenced"],
        report["reconcile"]["objects_referenced"],
        "the counter and the reconcile block are the same number: {report}"
    );
    assert_eq!(
        report["cost"]["counters"]["reconcile.orphan_objects"],
        0,
        "a clean round counts no orphans: {report}"
    );
    assert_eq!(
        report["reconcile"]["orphan_objects"],
        serde_json::json!([]),
        "a clean round has no orphans: {report}"
    );
    // The per-file reports no longer carry a store-wide walk.
    for source in report["sources"].as_array().expect("sources") {
        assert!(
            source["cost"]["phases"].get("reconcile").is_none(),
            "a captured file does not walk the object store: {source}"
        );
    }
    assert!(
        report["cost"]["phases"].get("reconcile").is_some(),
        "the round reports its reconcile stage: {report}"
    );
    assert!(
        report["cost"]["phases"].get("ledger_flush").is_some(),
        "the round reports its ledger flush: {report}"
    );
}

#[test]
fn a_single_source_capture_still_reconciles_and_reports_orphans() {
    // The single-source command keeps its old shape: one reconcile, one
    // `orphan_objects` list in the capture report.
    let env = TestEnv::new();
    let root = env.root("round-single");
    Archive::init(&root).expect("init runtime root");
    let archive = Archive::open(&root).expect("archive opens");
    let claude = claude_root(&env, 1);
    let file = claude.join("proj").join("11111111-1111-4111-8111-000000000000.jsonl");
    let report = capture_source(&archive, &file, "claude", DOMAIN, None).expect("capture");
    assert!(report.ok, "{report:?}");
    assert!(
        report.orphan_objects.is_empty(),
        "a clean single capture reports no orphans: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. fill_before: one read per evidence object for multi-table changes.
// ---------------------------------------------------------------------------

#[test]
fn multi_table_updates_share_one_delta_batch_read() {
    let env = TestEnv::new();
    let root = env.root("round-evidence");
    Archive::init(&root).expect("init runtime root");
    let archive = Archive::open(&root).expect("archive opens");
    let fixture = support::build_v1(&env);

    let baseline = capture_source(&archive, &fixture.db, "opencode", DOMAIN, None)
        .expect("baseline capture");
    assert!(baseline.ok, "{baseline:?}");

    // Change row content in two whitelisted tables: every one of those rows
    // becomes an `update`, so the before images span two tables and one batch.
    {
        let conn = rusqlite::Connection::open(&fixture.db).expect("open fixture db");
        conn.execute(
            "UPDATE message SET data = '{\"role\":\"assistant\",\"changed\":1}',              time_updated = time_updated + 1000              WHERE id = (SELECT id FROM message ORDER BY id LIMIT 1)",
            [],
        )
        .expect("update message rows");
        conn.execute(
            "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"changed\"}',              time_updated = time_updated + 1000              WHERE id = (SELECT id FROM part ORDER BY id LIMIT 1)",
            [],
        )
        .expect("update part rows");
    }

    // Second capture: the changes become delta batch #1 (both tables in one
    // batch object).
    let first_delta = capture_source(&archive, &fixture.db, "opencode", DOMAIN, None)
        .expect("first delta capture");
    assert!(first_delta.ok, "{first_delta:?}");
    assert_eq!(
        first_delta.sqlite.as_ref().map(|info| info.mode.as_str()),
        Some("delta"),
        "the second capture is a delta: {first_delta:?}"
    );

    // Third capture: the before images of the new changes must come from that
    // batch, and the pass may open it only once although it carries two tables.
    {
        let conn = rusqlite::Connection::open(&fixture.db).expect("open fixture db");
        conn.execute(
            "UPDATE message SET data = '{\"role\":\"assistant\",\"changed\":2}',              time_updated = time_updated + 1000              WHERE id = (SELECT id FROM message ORDER BY id LIMIT 1)",
            [],
        )
        .expect("update message rows again");
        conn.execute(
            "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"changed-2\"}',              time_updated = time_updated + 1000              WHERE id = (SELECT id FROM part ORDER BY id LIMIT 1)",
            [],
        )
        .expect("update part rows again");
    }
    cost::begin_recording();
    let delta = capture_source(&archive, &fixture.db, "opencode", DOMAIN, None)
        .expect("delta capture");
    let cost = cost::finish_recording().expect("recording");
    assert!(delta.ok, "{delta:?}");
    let counters = &cost.counters;
    let delta_batches = counters
        .get("evidence_read.delta_batches")
        .copied()
        .unwrap_or(0);
    let object_reads = counters
        .get("evidence_read.object_reads")
        .copied()
        .unwrap_or(0);
    let distinct = counters
        .get("evidence_read.distinct_objects")
        .copied()
        .unwrap_or(0);
    let baseline_chunks = counters
        .get("evidence_read.baseline_chunks")
        .copied()
        .unwrap_or(0);
    assert!(
        delta_batches >= 1,
        "the delta batch is read: {:?} {counters:?}",
        delta.sqlite
    );
    assert!(
        delta_batches <= 1,
        "one batch object is read once for every table it touches, \
         not once per table: {counters:?}"
    );
    assert_eq!(
        object_reads, distinct,
        "no evidence object is decompressed twice: {counters:?}"
    );
    assert_eq!(
        object_reads,
        delta_batches + baseline_chunks,
        "the read count is exactly the hit objects: {counters:?}"
    );
    assert!(
        counters
            .get("evidence_read.delta_bytes")
            .copied()
            .unwrap_or(0)
            > 0,
        "the delta payload is decompressed once: {counters:?}"
    );
}
