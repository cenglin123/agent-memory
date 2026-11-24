//! implementation-005 rework (§A): the cost shape of the committed-manifest
//! snapshot build.
//!
//! A build is one directory listing, one read+parse per manifest file and **one
//! table insert per committed object reference**; the record index is built only
//! when a record is actually checked. The assertions are counter based
//! (`archive::snapshot_stats`), so they hold on any machine and on any scheduler:
//!
//! - `builds` / `parsed` follow the manifest count, never the number of reads;
//! - `object_refs` equals the committed object references (one insert each) and
//!   `insert_probes <= 2 x object_refs` — a log-time index (the `BTreeMap` this
//!   replaced) probes tens of slots per insert, an open-addressed table probes a
//!   handful whatever the table size;
//! - `record_refs` stays 0 until a record access check needs the index, then it
//!   equals the committed record references and `record_index_builds` is 1;
//! - an append-only refresh parses only the new manifests.
//!
//! Timings are printed (run with `--nocapture`) for the record; only a generous
//! wall-clock ceiling guards the degenerate case, so a slow machine cannot make
//! the test flaky. The production-scale measurement is the `#[ignore]`d test:
//!
//! `cargo test --release --test manifest_build_cost -- --ignored --nocapture`

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use agent_memory::archive::{manifest_parse_count, snapshot_stats, Archive, Manifest, RecordRef};
use serde_json::json;

mod support;

use support::TestEnv;

const DOMAIN: &str = "default";

/// The snapshot counters are process-wide: the cases of this binary must not
/// interleave their `snapshot_stats()` deltas, so each test body holds this lock
/// for its whole measurement.
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn counter_lock() -> std::sync::MutexGuard<'static, ()> {
    COUNTER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A 64-hex-character sha that is distinct per `(prefix, index)`.
fn sha(prefix: u64, index: u64) -> String {
    format!("{prefix:016x}{index:048x}")
}

fn manifest(seq: usize, objects: usize, records: usize) -> Manifest {
    let capture_id = format!("cap-{seq:08}");
    let object_shas: Vec<String> = (0..objects)
        .map(|index| sha(seq as u64 + 1, index as u64))
        .collect();
    serde_json::from_value(json!({
        "schema": "agent-memory/manifest@1",
        "capture_id": capture_id,
        "captured_at_ms": 1_700_000_000_000i64 + seq as i64,
        "device_id": "build-cost-device",
        "provider": "opencode",
        "source": {
            "kind": "sqlite",
            "path": format!("/synthetic/source/{}", seq % 8),
            "fingerprint": {
                "mtime_ms": 1_700_000_000_000i64,
                "len": 1_048_576u64,
                "head_sha256": sha(seq as u64 + 1, 0),
                "tail_sha256": sha(seq as u64 + 1, 1)
            },
            "source_state_at_capture": sha(seq as u64 + 1, 2),
            "capture_reason": "append",
            "generation": 1
        },
        "objects": object_shas
            .iter()
            .enumerate()
            .map(|(index, sha256)| json!({
                "sha256": sha256,
                "bytes": 4096,
                "role": "sqlite_delta_batch",
                "access": ["default"],
                "source_offset": index
            }))
            .collect::<Vec<_>>(),
        "records": (0..records)
            .map(|index| json!({
                "seq": index,
                "offset": index,
                "len": 100,
                "sha256": sha(1_000_000 + seq as u64, index as u64),
                "object": object_shas[index % object_shas.len().max(1)],
                "access": ["default"],
                "source_line": index + 1
            }))
            .collect::<Vec<_>>()
    }))
    .expect("synthetic manifest")
}

/// Write `count` synthetic manifests into the archive's manifest directory.
fn write_manifests(dir: &Path, count: usize, objects: usize, records: usize) {
    fs::create_dir_all(dir).expect("manifest dir");
    for seq in 0..count {
        let value = manifest(seq, objects, records);
        let name = format!("{:08}-{}.json", seq + 1, value.capture_id);
        fs::write(
            dir.join(name),
            serde_json::to_string_pretty(&value).expect("serialize"),
        )
        .expect("write manifest");
    }
}

fn manifest_dir(root: &Path) -> std::path::PathBuf {
    root.join("archive").join("manifests")
}

/// One measured build.
struct Build {
    elapsed: Duration,
    parsed: u64,
    snapshot_len: usize,
}

fn build(root: &Path, count: usize, objects: usize, records: usize) -> Build {
    write_manifests(&manifest_dir(root), count, objects, records);
    let archive = Archive::open(root).expect("archive opens");
    let parsed_before = manifest_parse_count();
    let started = Instant::now();
    let snapshot = archive.manifests().expect("snapshot");
    Build {
        elapsed: started.elapsed(),
        parsed: manifest_parse_count() - parsed_before,
        snapshot_len: snapshot.len(),
    }
}

/// Two scales of the same shape: the second is four times the first. Both must
/// show one insert per reference and a bounded number of probes.
#[test]
fn the_build_is_linear_in_the_committed_references() {
    let _guard = counter_lock();
    let env = TestEnv::new();
    let mut per_reference_probes = Vec::new();
    for (scale, count) in [("small", 64usize), ("large", 256usize)] {
        let objects = 100usize;
        let records = 200usize;
        let root = env.root(&format!("build-cost-{scale}"));
        Archive::init(&root).expect("init runtime root");
        let before = snapshot_stats();
        let measured = build(&root, count, objects, records);
        let after = snapshot_stats();

        // One build, one listing, one parse per manifest file: the number of
        // reads does not move these.
        assert_eq!(after.builds - before.builds, 1, "{scale}: one build");
        assert_eq!(after.lists - before.lists, 1, "{scale}: one listing");
        assert_eq!(
            measured.parsed, count as u64,
            "{scale}: one parse per manifest file"
        );
        assert_eq!(measured.snapshot_len, count, "{scale}: every manifest");

        // One table insert per committed object reference, and nothing for the
        // record side until a record is checked.
        let object_refs = (count * objects) as u64;
        assert_eq!(
            after.object_refs - before.object_refs,
            object_refs,
            "{scale}: one insert per object reference"
        );
        assert_eq!(
            after.record_refs - before.record_refs,
            0,
            "{scale}: a capture-shaped build indexes no record"
        );
        let probes = after.insert_probes - before.insert_probes;
        assert!(
            probes <= object_refs * 2,
            "{scale}: {probes} probes for {object_refs} inserts is not linear"
        );
        per_reference_probes.push(probes as f64 / object_refs as f64);

        // A generous ceiling: the interesting assertion is the counter above.
        assert!(
            measured.elapsed < Duration::from_secs(120),
            "{scale}: the build took {:?}",
            measured.elapsed
        );
        println!(
            "build {scale}: manifests={count} refs={object_refs} parsed={} build_ms={} probes_per_insert={:.3}",
            measured.parsed,
            measured.elapsed.as_millis(),
            per_reference_probes.last().copied().unwrap_or_default()
        );
    }
    // The load factor keeps the probe count flat as the table grows four times:
    // a log-time search would grow with log(4 x n) / log(n).
    assert!(
        per_reference_probes[1] <= per_reference_probes[0] * 1.5 + 0.2,
        "probes per insert grew with the table: {per_reference_probes:?}"
    );
}

/// The record index is built on the first record check and reused afterwards; the
/// decision is the fail-closed one of the per-read scan.
#[test]
fn the_record_index_is_built_on_the_first_record_check_only() {
    let _guard = counter_lock();
    let env = TestEnv::new();
    let root = env.root("build-cost-records");
    Archive::init(&root).expect("init runtime root");
    let (count, objects, records) = (48usize, 10usize, 40usize);
    let measured = build(&root, count, objects, records);
    assert_eq!(measured.snapshot_len, count);
    let archive = Archive::open(&root).expect("archive opens");
    let snapshot = archive.manifests().expect("snapshot");
    let carrier = snapshot
        .iter()
        .find(|manifest| !manifest.records.is_empty())
        .expect("a manifest with records");
    let record = carrier.records[0].clone();
    let reference = RecordRef {
        object: record.object.clone(),
        offset: record.offset,
        len: record.len,
        capture_id: carrier.capture_id.clone(),
    };

    let before = snapshot_stats();
    for round in 0..2 {
        assert!(
            snapshot
                .check_record(&record.sha256, &reference, DOMAIN)
                .expect("record readable"),
            "round {round}: a sqlite carrier reports the compressed batch payload"
        );
    }
    let after = snapshot_stats();
    assert_eq!(
        after.record_index_builds - before.record_index_builds,
        1,
        "the record index is built once, on the first check"
    );
    assert_eq!(
        after.record_refs - before.record_refs,
        (count * records) as u64,
        "every committed record reference is indexed once"
    );
    assert!(
        after.insert_probes - before.insert_probes <= (count * records) as u64 * 2,
        "the lazy record build is linear in the committed references"
    );
    match snapshot.check_record(&sha(999_999, 0), &reference, DOMAIN) {
        Err(agent_memory::archive::ArchiveError::AccessDenied(message)) => {
            assert!(message.contains("no committed reference"), "{message}")
        }
        other => panic!("expected a denial for an unknown record, got {other:?}"),
    }
}

/// An append-only refresh parses only the appended manifests and inserts only
/// their references: the snapshot is not rebuilt and not re-read.
#[test]
fn an_append_only_refresh_parses_only_the_new_manifests() {
    let _guard = counter_lock();
    let env = TestEnv::new();
    let root = env.root("build-cost-append");
    Archive::init(&root).expect("init runtime root");
    let (count, objects, records) = (40usize, 8usize, 16usize);
    build(&root, count, objects, records);
    let archive = Archive::open(&root).expect("archive opens");
    let first = archive.manifests().expect("snapshot");
    assert_eq!(first.len(), count);

    // Three manifests committed by another instance.
    for seq in count..count + 3 {
        let value = manifest(seq, objects, records);
        fs::write(
            manifest_dir(&root).join(format!("{:08}-{}.json", seq + 1, value.capture_id)),
            serde_json::to_string_pretty(&value).expect("serialize"),
        )
        .expect("write appended manifest");
    }
    let before = snapshot_stats();
    let parsed_before = manifest_parse_count();
    let refreshed = archive.manifests().expect("refreshed snapshot");
    let after = snapshot_stats();
    assert_eq!(refreshed.len(), count + 3, "the appended manifests appear");
    assert_eq!(
        manifest_parse_count() - parsed_before,
        3,
        "only the appended manifests are parsed"
    );
    assert_eq!(
        after.extensions - before.extensions,
        1,
        "one append-only extension"
    );
    assert_eq!(
        after.builds - before.builds,
        0,
        "an append-only refresh never rebuilds the snapshot"
    );
    assert_eq!(
        after.object_refs - before.object_refs,
        (3 * objects) as u64,
        "one insert per appended object reference"
    );
}

/// Production-shape measurement (thousands of manifests, hundreds of thousands of
/// committed references). Ignored: it writes ~250 MB of synthetic manifests.
#[test]
#[ignore = "production-scale measurement; run with --ignored --nocapture"]
fn build_cost_at_production_scale() {
    let _guard = counter_lock();
    let env = TestEnv::new();
    for (scale, count) in [("store", 800usize), ("production", 5000usize)] {
        let (objects, records) = (40usize, 120usize);
        let root = env.root(&format!("build-cost-{scale}"));
        Archive::init(&root).expect("init runtime root");
        let before = snapshot_stats();
        let measured = build(&root, count, objects, records);
        let after = snapshot_stats();
        let refs = (count * objects) as u64;
        println!(
            "production-scale {scale}: manifests={count} object_refs={refs} record_refs={} parsed={} build_ms={} probes_per_insert={:.3}",
            count * records,
            measured.parsed,
            measured.elapsed.as_millis(),
            (after.insert_probes - before.insert_probes) as f64 / refs as f64
        );
        assert_eq!(measured.snapshot_len, count);
        assert_eq!(measured.parsed, count as u64);
    }
}
