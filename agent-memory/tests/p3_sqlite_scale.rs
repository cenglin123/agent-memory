//! Stage-2 S1 scale test (`#[ignore]`): OpenCode-sized synthetic databases.
//!
//! Builds WAL-mode SQLite databases with 200k (and a doubled 400k-row variant)
//! `part` rows (2–5 KB payloads) and 50k `message` rows, then measures:
//!
//! - the first chunked baseline (wall time, uncompressed vs compressed bytes),
//! - a steady-state capture (100 updates + 100 inserts + 10 deletes) with wall
//!   time, peak process memory and the decompressed-evidence bytes (N4: the
//!   read volume must follow what changed, not the evidence size),
//! - an unchanged capture (N4: decompresses nothing),
//! - the full `--verify` pass at N and at 2× rows (N5: peak memory < 300 MiB
//!   and not growing significantly with the row count).
//!
//! Measurement method (documented for the completion record): wall time via
//! `std::time::Instant`; peak memory via a `#[global_allocator]` that counts
//! live allocated bytes and records the maximum observed inside the measured
//! section (this file contains exactly one test, so no concurrent pollution);
//! decompressed evidence bytes via the collector's own
//! `evidence_decompressed_bytes()` metric.
//!
//! Run: `cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use agent_memory::archive::Archive;
use agent_memory::capture::{capture_source_with_options, CaptureOptions};
use agent_memory::capture_sqlite::{
    evidence_decompressed_bytes, reset_evidence_decompressed_bytes,
};
use rusqlite::Connection;
use serde_json::json;

static TRACKING: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Tracking;

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() && TRACKING.load(Ordering::Relaxed) {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if TRACKING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        System.dealloc(pointer, layout)
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

fn peak_mib() -> f64 {
    PEAK.swap(0, Ordering::Relaxed) as f64 / (1024.0 * 1024.0)
}

fn start_tracking() {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
}

fn stop_tracking() {
    TRACKING.store(false, Ordering::Relaxed);
}

/// Synthetic OpenCode-shaped database: `part_count` parts × ~2.5 KB payloads,
/// 50k messages × ~1 KB payloads, WAL mode.
fn build_database(path: &std::path::Path, part_count: usize) {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT, parent_id TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY, title TEXT, parent_id TEXT, directory TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE session_message (
            id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT,
            FOREIGN KEY (session_id) REFERENCES session_v2(id) ON DELETE CASCADE
        );
        CREATE TABLE project (id TEXT PRIMARY KEY, directory TEXT);
        CREATE TABLE todo (id TEXT PRIMARY KEY, session_id TEXT, content TEXT);
        CREATE TABLE credential (id TEXT PRIMARY KEY, secret TEXT);
        INSERT INTO credential VALUES ('cred_1', 'SENTINEL-SECRET-scale');
        INSERT INTO session VALUES ('ses_scale', 'scale', NULL, 1, 2);
        "#,
    )
    .unwrap();

    let payload = |index: usize, width: usize| {
        // Deterministic ~`width`-byte JSON payload.
        let body = "x".repeat(width.saturating_sub(60 + index % 40));
        json!({"type":"text","text":format!("part {index}"), "body": body}).to_string()
    };

    let mut inserts = conn
        .prepare_cached("INSERT INTO part VALUES (?1, ?2, 'ses_scale', ?3, ?3, ?4)")
        .unwrap();
    for index in 0..part_count {
        inserts
            .execute(rusqlite::params![
                format!("prt_{index:06}"),
                format!("msg_{:05}", index / 4),
                1_000 + index as i64,
                payload(index, 2_500)
            ])
            .unwrap();
    }
    let mut messages = conn
        .prepare_cached("INSERT INTO message VALUES (?1, 'ses_scale', ?2, ?2, ?3)")
        .unwrap();
    for index in 0..50_000usize {
        messages
            .execute(rusqlite::params![
                format!("msg_{index:05}"),
                1_000 + index as i64,
                json!({"role":"assistant","text":"x".repeat(900)}).to_string()
            ])
            .unwrap();
    }
    drop(inserts);
    drop(messages);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
}

fn capture(archive: &Archive, db: &std::path::Path, options: CaptureOptions) -> agent_memory::capture::CaptureReport {
    capture_source_with_options(archive, db, "opencode", "default", None, options).unwrap()
}

#[test]
#[ignore = "scale case: builds ~1.6 GB of synthetic databases; run with --ignored"]
fn opencode_scale_capture_is_incremental_and_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let db = source_dir.join("opencode.db");

    let build_started = Instant::now();
    build_database(&db, 200_000);
    println!(
        "[build] {:?} db-file={} MiB",
        build_started.elapsed(),
        std::fs::metadata(&db).unwrap().len() / (1024 * 1024)
    );

    // --- First baseline: chunked export, one read transaction ----------------
    start_tracking();
    let started = Instant::now();
    let baseline = capture(&archive, &db, CaptureOptions::default());
    let baseline_elapsed = started.elapsed();
    stop_tracking();
    let baseline_peak_mib = peak_mib();
    let info = baseline.sqlite.as_ref().unwrap();
    let baseline_bytes = info.baseline_bytes;
    println!(
        "[baseline] {:?} rows={} chunks={} uncompressed={} KiB compressed={} KiB ratio={:.2} peak-heap={} MiB ok={}",
        baseline_elapsed,
        info.rows_discovered,
        info.baseline_chunks,
        baseline_bytes / 1024,
        baseline.bytes / 1024,
        baseline_bytes as f64 / baseline.bytes.max(1) as f64,
        baseline_peak_mib as u64,
        baseline.ok
    );
    assert!(baseline.ok, "{baseline:?}");
    assert_eq!(info.rows_discovered, 250_001);
    assert!(info.baseline_chunks >= 16, "a ~500 MiB export must chunk");
    let source_key = {
        let sources = archive.load_sources().unwrap();
        sources.sources.keys().next().unwrap().clone()
    };
    for baseline_projection in agent_memory::capture_sqlite::sqlite_baselines(&archive, &source_key).unwrap() {
        for chunk in &baseline_projection.chunks {
            assert!(
                !chunk.min_key.is_empty() || !chunk.max_key.is_empty(),
                "every chunk records its key range (N4 before-lookups)"
            );
        }
    }

    // --- Steady state: 100 updates + 100 inserts + 10 deletes ---------------
    {
        let conn = Connection::open(&db).unwrap();
        for index in 0..100usize {
            conn.execute(
                "UPDATE part SET data = ?2, time_updated = ?3 WHERE id = ?1",
                rusqlite::params![
                    format!("prt_{index:06}"),
                    json!({"type":"text","text":format!("updated {index}"), "body": "y".repeat(2400)}).to_string(),
                    5_000_000 + index as i64
                ],
            )
            .unwrap();
        }
        for index in 200_000..200_100usize {
            conn.execute(
                "INSERT INTO part VALUES (?1, 'msg_00000', 'ses_scale', ?2, ?2, ?3)",
                rusqlite::params![
                    format!("prt_new_{index:06}"),
                    5_100_000 + index as i64,
                    json!({"type":"text","text":"inserted", "body": "z".repeat(2400)}).to_string()
                ],
            )
            .unwrap();
        }
        for index in 199_990..200_000usize {
            conn.execute(
                "DELETE FROM part WHERE id = ?1",
                rusqlite::params![format!("prt_{index:06}")],
            )
            .unwrap();
        }
    }
    reset_evidence_decompressed_bytes();
    start_tracking();
    let started = Instant::now();
    let steady = capture(&archive, &db, CaptureOptions::default());
    let steady_elapsed = started.elapsed();
    stop_tracking();
    let steady_peak_mib = peak_mib();
    let decompressed = evidence_decompressed_bytes();
    let steady_info = steady.sqlite.as_ref().unwrap();
    println!(
        "[steady] {:?} mode={} inserted={} updated={} deleted={} delta-bytes={} KiB decompressed-evidence={} KiB ({}% of baseline) peak-heap={} MiB ok={}",
        steady_elapsed,
        steady.change,
        steady_info.rows_inserted,
        steady_info.rows_updated,
        steady_info.rows_deleted,
        steady.bytes / 1024,
        decompressed / 1024,
        decompressed * 100 / baseline_bytes.max(1),
        steady_peak_mib as u64,
        steady.ok
    );
    assert!(steady.ok, "{steady:?}");
    assert_eq!(steady.change, "delta", "steady state must not re-baseline");
    assert_eq!(steady_info.rows_inserted, 100);
    assert_eq!(steady_info.rows_updated, 100);
    assert_eq!(steady_info.rows_deleted, 10);
    // N4: the evidence read volume is bounded by the touched chunks/batches,
    // not by the evidence size (two touched part chunks + nothing else).
    assert!(
        decompressed < baseline_bytes / 4,
        "steady-state decompressed {decompressed} B, baseline is {baseline_bytes} B"
    );

    // --- Unchanged observation: decompresses nothing (N4) --------------------
    reset_evidence_decompressed_bytes();
    let unchanged = capture(&archive, &db, CaptureOptions::default());
    assert_eq!(unchanged.change, "skipped_unchanged");
    assert_eq!(
        evidence_decompressed_bytes(),
        0,
        "an unchanged observation must not decompress any evidence object"
    );

    // --- Verification pass at N rows (N5) ------------------------------------
    start_tracking();
    let started = Instant::now();
    let verified = capture(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let verify_elapsed = started.elapsed();
    stop_tracking();
    let verify_peak_mib = peak_mib();
    let verify_info = verified.sqlite.as_ref().unwrap();
    println!(
        "[verify@200k] {:?} mismatches={} peak-heap={} MiB ok={}",
        verify_elapsed, verify_info.verify_mismatches, verify_peak_mib as u64, verified.ok
    );
    assert!(verified.ok, "{verified:?}");
    assert_eq!(verify_info.verify_mismatches, 0);

    // --- 2× rows: verify memory must not grow with the table -----------------
    let db2 = source_dir.join("opencode-double.db");
    std::fs::copy(&db, &db2).unwrap();
    {
        let conn = Connection::open(&db2).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        // Duplicate every part under a key that sorts after all existing ones.
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             SELECT 'zz' || id, message_id, session_id, time_created, time_updated, data FROM part",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let archive2 = Archive::init(&temp.path().join("evidence2")).unwrap();
    let baseline2 = capture(&archive2, &db2, CaptureOptions::default());
    assert!(baseline2.ok, "{baseline2:?}");
    let rows2 = baseline2.sqlite.as_ref().unwrap().rows_discovered;
    println!(
        "[baseline@2x] rows={} chunks={}",
        rows2,
        baseline2.sqlite.as_ref().unwrap().baseline_chunks
    );
    start_tracking();
    let started = Instant::now();
    let verified2 = capture(
        &archive2,
        &db2,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let verify2_elapsed = started.elapsed();
    stop_tracking();
    let verify2_peak_mib = peak_mib();
    let verify2_info = verified2.sqlite.as_ref().unwrap();
    println!(
        "[verify@400k] {:?} rows=2x mismatches={} peak-heap={} MiB growth={:+.0}% ok={}",
        verify2_elapsed,
        verify2_info.verify_mismatches,
        verify2_peak_mib as u64,
        (verify2_peak_mib - verify_peak_mib) * 100.0 / verify_peak_mib.max(1.0),
        verified2.ok
    );
    assert!(verified2.ok, "{verified2:?}");
    assert_eq!(verify2_info.verify_mismatches, 0);

    // --- Budgets --------------------------------------------------------------
    println!(
        "[budget] steady {:?} ≤ 10 s: {} | peak {:.0} MiB < 300 MiB: {} | verify@200k {:.0} MiB < 300 MiB: {} | verify@400k {:.0} MiB < 300 MiB: {} | growth {} MiB ≤ 100 MiB: {}",
        steady_elapsed,
        steady_elapsed.as_secs() < 10,
        steady_peak_mib,
        steady_peak_mib < 300.0,
        verify_peak_mib,
        verify_peak_mib < 300.0,
        verify2_peak_mib,
        verify2_peak_mib < 300.0,
        verify2_peak_mib - verify_peak_mib,
        verify2_peak_mib - verify_peak_mib <= 100.0
    );
    assert!(
        steady_elapsed.as_secs() < 10,
        "steady-state capture took {steady_elapsed:?} (budget 10 s)"
    );
    assert!(
        steady_peak_mib < 300.0,
        "steady-state peak heap {:.0} MiB (budget 300 MiB)",
        steady_peak_mib
    );
    assert!(
        verify_peak_mib < 300.0,
        "verify peak heap {:.0} MiB at 250k rows (budget 300 MiB)",
        verify_peak_mib
    );
    assert!(
        verify2_peak_mib < 300.0,
        "verify peak heap {:.0} MiB at 500k rows (budget 300 MiB)",
        verify2_peak_mib
    );
    assert!(
        verify2_peak_mib - verify_peak_mib <= 100.0,
        "verify peak grew {} MiB when the row count doubled (must not scale with rows)",
        verify2_peak_mib - verify_peak_mib
    );
}
