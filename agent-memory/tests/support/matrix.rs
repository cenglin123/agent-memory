//! §G regression foundation: frozen-input scenario helpers.
//!
//! Two paths read the **same** frozen input:
//!
//! * *incremental* — an index cache that is refreshed round by round while the
//!   input changes;
//! * *rebuild* — a second cache built with `rebuild = true` over the final
//!   frozen input.
//!
//! [`assert_equivalent`] compares the normalised digests of both caches
//! (`agent_memory::recall::fulltext::digest`), and [`diff`] names the differing
//! table/row. [`ledger_oracle`] replays the committed manifests in capture
//! order and compares the result with the maintained `state/records.json`
//! (first-writer-wins, like `capture_jsonl`'s `or_insert`).
//!
//! Everything here is synthetic: no test points at a real session directory,
//! database or Archive.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_memory::archive::Archive;
use agent_memory::recall::config::RecallConfig;
use agent_memory::recall::fulltext::digest::{self, SourceDigest};
use serde_json::Value;

/// The compiled CLI (capture / index / index-digest subprocess cases).
pub fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

pub struct CliRun {
    pub code: i32,
    pub report: Value,
    pub stdout: String,
    pub stderr: String,
}

/// A run whose stdout is not JSON (the text form of `index-digest`).
pub struct RawRun {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn run_raw(args: &[&str]) -> RawRun {
    let out = cli().args(args).output().expect("run agent-memory");
    RawRun {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8(out.stdout).expect("stdout utf-8"),
        stderr: String::from_utf8(out.stderr).expect("stderr utf-8"),
    }
}

pub fn run(args: &[&str]) -> CliRun {
    let out = cli().args(args).output().expect("run agent-memory");
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf-8");
    let report = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("report is not JSON ({error}): {stdout} / {stderr}"));
    CliRun {
        code: out.status.code().unwrap_or(-1),
        report,
        stdout,
        stderr,
    }
}

// ---------------------------------------------------------------------------
// File timestamps (the cheap change signals the watermarks use)
// ---------------------------------------------------------------------------

pub fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .modified()
        .expect("mtime")
}

pub fn set_mtime(path: &Path, at: SystemTime) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|error| panic!("open {} for mtime: {error}", path.display()));
    file.set_times(fs::FileTimes::new().set_modified(at))
        .expect("set mtime");
}

pub fn ago(seconds: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(seconds)
}

/// A byte-level rewrite that keeps the file length (the `same_length_rewrite`
/// shape) — the caller supplies equal-length content.
pub fn rewrite_keep_len(path: &Path, bytes: &[u8]) {
    let before = fs::metadata(path).expect("stat").len();
    assert_eq!(before, bytes.len() as u64, "fixture must keep its length");
    fs::write(path, bytes).expect("rewrite fixture");
}

// ---------------------------------------------------------------------------
// Digest comparison
// ---------------------------------------------------------------------------

/// The `rows` and `newest_unit_ts` one index `meta` reports — the two numbers
/// `detail.coverage` shows. They are content facts, not build times, so an
/// incrementally maintained index must report exactly what a rebuild reports.
pub fn meta_counts(config: &RecallConfig, source_id: &str) -> Option<(u64, i64)> {
    let root = config.cache_root.as_deref()?;
    let path = agent_memory::recall::fulltext::index_path(Path::new(root), source_id);
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let meta = agent_memory::recall::fulltext::IndexMeta::load(&conn)
        .ok()
        .flatten()?;
    Some((meta.rows, meta.newest_unit_ts))
}

/// The normalised digest of every configured source under one cache root.
pub fn digest_all(config: &RecallConfig) -> Vec<SourceDigest> {
    digest::digest_sources(config, None)
}

/// Compare two caches over the same frozen input; panic with a per-table,
/// per-row report on a mismatch.
pub fn assert_equivalent(left: &RecallConfig, right: &RecallConfig, what: &str) {
    let left_sources = digest_all(left);
    let right_sources = digest_all(right);
    assert_eq!(
        left_sources.len(),
        right_sources.len(),
        "{what}: source count differs"
    );
    let mut problems: Vec<String> = Vec::new();
    for (a, b) in left_sources.iter().zip(right_sources.iter()) {
        assert_eq!(a.source_id, b.source_id, "{what}: source order differs");
        // A source kind without a live index (kb, archive) is configured but
        // never built: there is nothing to compare, on either side.
        if a.status == "not_indexable" && b.status == "not_indexable" {
            continue;
        }
        // The rebuild side must be a usable index, never a degraded status: an
        // equivalence check between two non-ready indexes would pass while
        // proving nothing.
        assert_eq!(
            b.status, "ready",
            "{what}: the rebuild-side source {} is {} ({:?}), not a usable index",
            b.source_id, b.status, b.reason
        );
        if a.status != b.status {
            problems.push(format!(
                "{}: status {} != {} ({:?} / {:?})",
                a.source_id, a.status, b.status, a.reason, b.reason
            ));
            continue;
        }
        if a.status != "ready" {
            problems.push(format!(
                "{}: the incremental side is {} ({:?}), not a usable index",
                a.source_id, a.status, a.reason
            ));
            continue;
        }
        for line in digest::diff(a, b) {
            problems.push(format!("{}: {line}", a.source_id));
        }
    }
    assert!(
        problems.is_empty(),
        "{what}: incremental index differs from a rebuild:\n{}",
        problems.join("\n")
    );
    // The reported counters: `rows` is a content fact (equal on both sides), and
    // `newest_unit_ts` is **monotone per index** (it never goes backwards, see
    // `finish_counts`), so the increment must be at least a rebuild's value —
    // a deleted newest row legitimately leaves the increment higher.
    for source in left.sources.iter().filter(|source| source.kind.is_indexable()) {
        let a = meta_counts(left, &source.source_id);
        let b = meta_counts(right, &source.source_id);
        if a.is_none() && b.is_none() {
            continue;
        }
        let (a, b) = (a.expect("left meta"), b.expect("right meta"));
        assert_eq!(
            a.0, b.0,
            "{what}: {} reports {} units but a rebuild reports {}",
            source.source_id, a.0, b.0
        );
        assert!(
            a.1 >= b.1,
            "{what}: {} reports newest_unit_ts {} below a rebuild's {} (the counter must never regress)",
            source.source_id,
            a.1,
            b.1
        );
    }
}

/// The digest of one source, for tests that assert a *known gap* (the two
/// paths must still differ there).
pub fn digest_one(config: &RecallConfig, source_id: &str) -> SourceDigest {
    digest::digest_sources(config, Some(source_id))
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("source {source_id} missing from the digests"))
}

// ---------------------------------------------------------------------------
// Cost counters
// ---------------------------------------------------------------------------

pub fn counter(cost: &Value, name: &str) -> u64 {
    cost["counters"][name].as_u64().unwrap_or(0)
}

pub fn phase(cost: &Value, name: &str) -> bool {
    cost["phases"].get(name).is_some()
}

/// Total of every counter whose name starts with `prefix` (for a family such as
/// `index.full_rescan.`).
pub fn counter_sum(cost: &Value, prefix: &str) -> u64 {
    cost["counters"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(name, _)| name.starts_with(prefix))
                .filter_map(|(_, value)| value.as_u64())
                .sum()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Record ledger oracle
// ---------------------------------------------------------------------------

/// `sha → (object, offset, len, capture_id)` replayed from the committed
/// manifests in capture order with first-writer-wins semantics.
pub fn replayed_records(root: &Path) -> BTreeMap<String, (String, u64, u64, String)> {
    let archive = Archive::open(root).expect("open archive");
    let mut out: BTreeMap<String, (String, u64, u64, String)> = BTreeMap::new();
    for manifest in archive.manifests().expect("manifests").iter() {
        for record in &manifest.records {
            out.entry(record.sha256.clone()).or_insert_with(|| {
                (
                    record.object.clone(),
                    record.offset,
                    record.len,
                    manifest.capture_id.clone(),
                )
            });
        }
    }
    out
}

/// The maintained ledger, in the same canonical shape.
pub fn maintained_records(root: &Path) -> BTreeMap<String, (String, u64, u64, String)> {
    let archive = Archive::open(root).expect("open archive");
    archive
        .load_records()
        .expect("records")
        .records
        .into_iter()
        .map(|(sha, reference)| {
            (
                sha,
                (
                    reference.object,
                    reference.offset,
                    reference.len,
                    reference.capture_id,
                ),
            )
        })
        .collect()
}

/// Assert `records.json` equals the manifest replay, naming the first
/// difference.
pub fn assert_ledger_matches_manifests(root: &Path, what: &str) {
    let replayed = replayed_records(root);
    let maintained = maintained_records(root);
    if replayed == maintained {
        return;
    }
    let missing: Vec<&String> = replayed
        .keys()
        .filter(|sha| !maintained.contains_key(*sha))
        .collect();
    let extra: Vec<&String> = maintained
        .keys()
        .filter(|sha| !replayed.contains_key(*sha))
        .collect();
    let changed: Vec<&String> = replayed
        .iter()
        .filter(|(sha, value)| maintained.get(*sha).is_some_and(|other| other != *value))
        .map(|(sha, _)| sha)
        .collect();
    panic!(
        "{what}: records.json differs from the manifest replay (missing {}, extra {}, changed {}); \
         first missing {:?}, first extra {:?}, first changed {:?}",
        missing.len(),
        extra.len(),
        changed.len(),
        missing.first(),
        extra.first(),
        changed.first()
    );
}

// ---------------------------------------------------------------------------
// Scenario scaffold
// ---------------------------------------------------------------------------

/// Two caches over one frozen input root.
pub struct Pair {
    pub incremental_root: PathBuf,
    pub rebuild_root: PathBuf,
}

impl Pair {
    pub fn new(env: &super::TestEnv, name: &str) -> Pair {
        Pair {
            incremental_root: env.root(&format!("{name}-incremental")),
            rebuild_root: env.root(&format!("{name}-rebuild")),
        }
    }
}

/// Run the incremental path over the current input state.
pub fn index_incremental(config: &RecallConfig) {
    let reports = agent_memory::recall::fulltext::build::index_all(config, None, false)
        .expect("incremental index");
    assert!(
        reports.iter().all(|report| {
            matches!(
                report.status.as_str(),
                "built" | "updated" | "unchanged" | "skipped_disabled" | "skipped_not_indexable"
            )
        }),
        "incremental index failed: {reports:?}"
    );
}

/// Run the rebuild path over the final frozen input.
pub fn index_rebuild(config: &RecallConfig) {
    let reports = agent_memory::recall::fulltext::build::index_all(config, None, true)
        .expect("rebuild index");
    assert!(
        reports.iter().all(|report| {
            matches!(
                report.status.as_str(),
                "built" | "updated" | "unchanged" | "skipped_disabled" | "skipped_not_indexable"
            )
        }),
        "rebuild failed: {reports:?}"
    );
}

/// Nanoseconds since the epoch for a `SystemTime` (the index watermarks' unit).
pub fn nanos(at: SystemTime) -> u128 {
    at.duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Vector cache layout downgrade (the upgrade-round scenario)
// ---------------------------------------------------------------------------

/// Length of one `u16`-prefixed string at `at` (prefix included).
fn prefixed_len(bytes: &[u8], at: usize) -> usize {
    let len = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
    2 + len
}

/// Rewrite a `vectors.bin` into the layout written before implementation-004a:
/// the same header and the same rows, without the per-session block (version 2).
///
/// The rows are content-addressed by their embedding-input hash, so the upgrade
/// round must reuse every one of them instead of re-embedding.
pub fn downgrade_vectors_to_previous_layout(path: &Path) {
    let bytes = fs::read(path).expect("read vectors");
    assert_eq!(&bytes[..4], b"AMLV", "vectors magic");
    assert_eq!(
        u16::from_le_bytes([bytes[4], bytes[5]]),
        3,
        "the helper writes the layout before version 3"
    );
    let mut out = bytes[..6].to_vec();
    out[4..6].copy_from_slice(&2u16.to_le_bytes());
    // generation key, quantization, projector version
    let mut cursor = 6;
    for _ in 0..3 {
        let next = cursor + prefixed_len(&bytes, cursor);
        out.extend_from_slice(&bytes[cursor..next]);
        cursor = next;
    }
    // dimension, row count
    out.extend_from_slice(&bytes[cursor..cursor + 8]);
    cursor += 8;
    let sessions = u32::from_le_bytes([
        bytes[cursor],
        bytes[cursor + 1],
        bytes[cursor + 2],
        bytes[cursor + 3],
    ]) as usize;
    cursor += 4;
    for _ in 0..sessions {
        cursor += prefixed_len(&bytes, cursor);
        cursor += prefixed_len(&bytes, cursor);
    }
    out.extend_from_slice(&bytes[cursor..]);
    fs::write(path, out).expect("write the previous layout");
}
