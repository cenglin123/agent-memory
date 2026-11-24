//! §A measurement: process I/O counters, phase timing and Windows background
//! mode.
//!
//! Everything here is **additive**: the measurement never changes evidence,
//! index content or a retrieval result. Counters are sampled only at phase
//! boundaries (`GetProcessIoCounters` is a cheap kernel query), so a long loop
//! pays two samples, not one per row.
//!
//! # Report shape
//!
//! `cost` is `{elapsed_ms, read_bytes, write_bytes, read_ops}` plus, when the
//! counters are unavailable (non-Windows, or the call failed), a
//! `cost_unavailable` reason with the I/O fields `null` — the command still
//! succeeds. Per-stage breakdowns appear as `cost.phases` on the objects that
//! have stages (capture sources, index sources); phase names are stable
//! snake_case:
//!
//! capture, SQLite source: `watermark_read` (plus `watermark_read.tables.<table>`
//! per table), `key_compare`, `session_filter_read`, `excluded_table_count`,
//! `verify`, `evidence_write`, `evidence_read`, `observation_state`,
//! `manifest_read`, `reconcile`.
//!
//! capture, JSONL / DSH sources: `enumerate`, `read`, `parse` (including the
//! damaged-line pre-check), `hash`, `compress_write`, `parse_probe`,
//! `reconcile`.
//!
//! capture, every source: `records_read` / `records_write` (the record ledger;
//! recorded inside the archive so every ledger touch is covered).
//!
//! `index`, per source: `live_scan`, `archive_manifest_parse`,
//! `archive_carrier_rebuild`, `archive_projection`, `vectors`.
//!
//! # Counters
//!
//! `cost.counters` carries plain integer counters with the same stable
//! snake_case names (`evidence_read.baseline_chunks`,
//! `watermark_rows.<table>`, `index.full_rescan.<reason>`,
//! `vectors.units_read`, …). A counter is a normal integer addition, never a
//! phase guard: the hot loops accumulate locally and add once at the end.
//! Sub-phases (`watermark_read.tables.<table>`) are contained in their parent
//! (`watermark_read`), so summing every phase would double count.
//!
//! # Background mode
//!
//! `--background` lowers the process priority (`PROCESS_MODE_BACKGROUND_BEGIN`:
//! lower CPU, I/O and memory priority) and the guard's `Drop` pairs it with
//! `PROCESS_MODE_BACKGROUND_END` on every exit path, including errors. A
//! failure to enter or leave the mode is reported, never raised.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;

/// `PROCESS_MODE_BACKGROUND_BEGIN` / `PROCESS_MODE_BACKGROUND_END`
/// (`winbase.h`). Both are priority *class* values for `SetPriorityClass`.
pub const PROCESS_MODE_BACKGROUND_BEGIN: u32 = 0x0010_0000;
pub const PROCESS_MODE_BACKGROUND_END: u32 = 0x0020_0000;

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// One cost sample (`cost` in the CLI reports).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Cost {
    pub elapsed_ms: u64,
    /// Process logical read bytes (`null` when the counters are unavailable).
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    /// Read operation count (`ReadOperationCount`).
    pub read_ops: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_unavailable: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub phases: BTreeMap<String, PhaseCost>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub counters: BTreeMap<String, u64>,
}

impl Cost {
    /// A cost sample without stage detail (used for whole-command reports).
    pub fn sample(elapsed_ms: u64, start: Option<IoCounters>, end: Option<IoCounters>) -> Cost {
        let mut cost = Cost {
            elapsed_ms,
            ..Cost::default()
        };
        match (start, end) {
            (Some(start), Some(end)) => {
                cost.read_bytes = Some(end.read_bytes.saturating_sub(start.read_bytes));
                cost.write_bytes = Some(end.write_bytes.saturating_sub(start.write_bytes));
                cost.read_ops = Some(end.read_ops.saturating_sub(start.read_ops));
            }
            (None, _) => cost.cost_unavailable = Some(unavailable_reason()),
            (Some(_), None) => cost.cost_unavailable = Some(unavailable_reason()),
        }
        cost
    }
}

/// One phase sample. `elapsed_ms` is the phase's wall time; the I/O fields are
/// that phase's share of the process counters (`null` when unavailable).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PhaseCost {
    pub elapsed_ms: u64,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    pub read_ops: Option<u64>,
}

/// One raw `GetProcessIoCounters` sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoCounters {
    pub read_ops: u64,
    pub write_ops: u64,
    pub other_ops: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub other_bytes: u64,
}

fn unavailable_reason() -> String {
    if cfg!(windows) {
        "GetProcessIoCounters failed".to_string()
    } else {
        "process I/O counters are only available on Windows".to_string()
    }
}

// ---------------------------------------------------------------------------
// Phase recording (per source build / capture, one thread)
// ---------------------------------------------------------------------------

struct Recorder {
    started: Instant,
    start_io: Option<IoCounters>,
    phases: BTreeMap<String, PhaseCost>,
    counters: BTreeMap<String, u64>,
}

thread_local! {
    static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
}

/// Start (or restart) phase recording for the current thread. A build that is
/// already recording is replaced: one recorder per source/run.
pub fn begin_recording() {
    let start_io = sample_io().ok();
    RECORDER.with(|slot| {
        *slot.borrow_mut() = Some(Recorder {
            started: Instant::now(),
            start_io,
            phases: BTreeMap::new(),
            counters: BTreeMap::new(),
        })
    });
}

fn recording() -> bool {
    RECORDER.with(|slot| slot.borrow().is_some())
}

/// Take the recorded cost and stop recording. Returns `None` when nothing was
/// being recorded.
pub fn finish_recording() -> Option<Cost> {
    RECORDER
        .with(|slot| slot.borrow_mut().take())
        .map(|recorder| {
            let elapsed_ms = recorder.started.elapsed().as_millis() as u64;
            let end_io = sample_io().ok();
            let mut cost = Cost::sample(elapsed_ms, recorder.start_io, end_io);
            cost.phases = recorder.phases;
            cost.counters = recorder.counters;
            cost
        })
}

/// A recording guard for one phase. Inactive (cheap no-op) unless a recorder is
/// running, so an uninstrumented test pays nothing.
/// True while this thread records phases (callers use it to skip per-line
/// clock reads in hot loops).
pub fn is_recording() -> bool {
    recording()
}

pub struct PhaseGuard {
    name: String,
    started: Option<Instant>,
    start_io: Option<IoCounters>,
}

impl PhaseGuard {
    fn inactive() -> PhaseGuard {
        PhaseGuard {
            name: String::new(),
            started: None,
            start_io: None,
        }
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let end_io = sample_io().ok();
        let delta = match (self.start_io, end_io) {
            (Some(start), Some(end)) => Some((
                end.read_bytes.saturating_sub(start.read_bytes),
                end.write_bytes.saturating_sub(start.write_bytes),
                end.read_ops.saturating_sub(start.read_ops),
            )),
            _ => None,
        };
        add_phase(self.name.as_str(), elapsed_ms, delta);
    }
}

fn add_phase(name: &str, elapsed_ms: u64, delta: Option<(u64, u64, u64)>) {
    RECORDER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(recorder) = slot.as_mut() else {
            return;
        };
        let entry = recorder.phases.entry(name.to_string()).or_default();
        entry.elapsed_ms = entry.elapsed_ms.saturating_add(elapsed_ms);
        if let Some((read_bytes, write_bytes, read_ops)) = delta {
            *entry.read_bytes.get_or_insert(0) += read_bytes;
            *entry.write_bytes.get_or_insert(0) += write_bytes;
            *entry.read_ops.get_or_insert(0) += read_ops;
        }
    });
}

/// Open a phase. The guard records its wall time (and I/O share) when dropped.
///
/// A dynamic name (for example one per table) is accepted; callers name phases
/// at a *stage* boundary, never per row.
pub fn phase(name: impl Into<String>) -> PhaseGuard {
    if !recording() {
        return PhaseGuard::inactive();
    }
    PhaseGuard {
        name: name.into(),
        started: Some(Instant::now()),
        start_io: sample_io().ok(),
    }
}

/// Add an already-measured duration to a phase (used where a phase is
/// interleaved with another one per line and a guard per line would be too
/// costly). Records time only; the enclosing phase carries the I/O delta.
pub fn record_duration(name: impl Into<String>, elapsed: std::time::Duration) {
    if !recording() {
        return;
    }
    add_phase(name.into().as_str(), elapsed.as_millis() as u64, None);
}

/// Run `work` and sample its cost as a standalone [`PhaseCost`].
///
/// For round-level stages that run *between* per-source recordings (the ledger
/// flush and the one reconcile of a capture-all round), where no recorder is
/// active and [`phase`] would be a no-op.
pub fn timed<T>(work: impl FnOnce() -> T) -> (T, PhaseCost) {
    let started = Instant::now();
    let start_io = sample_io().ok();
    let value = work();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let end_io = sample_io().ok();
    let phase = match (start_io, end_io) {
        (Some(start), Some(end)) => PhaseCost {
            elapsed_ms,
            read_bytes: Some(end.read_bytes.saturating_sub(start.read_bytes)),
            write_bytes: Some(end.write_bytes.saturating_sub(start.write_bytes)),
            read_ops: Some(end.read_ops.saturating_sub(start.read_ops)),
        },
        _ => PhaseCost {
            elapsed_ms,
            ..PhaseCost::default()
        },
    };
    (value, phase)
}

/// Run `work` inside a named phase.
pub fn measure<T>(name: impl Into<String>, work: impl FnOnce() -> T) -> T {
    let _guard = phase(name);
    work()
}

/// Add an integer counter (`name` stable snake_case). A counter is a plain
/// addition: cheap enough for a hot loop's local accumulator, and a no-op when
/// nothing records.
pub fn counter(name: &str, delta: u64) {
    if !recording() {
        return;
    }
    RECORDER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(recorder) = slot.as_mut() else {
            return;
        };
        let entry = recorder.counters.entry(name.to_string()).or_insert(0);
        *entry = entry.saturating_add(delta);
    });
}

// ---------------------------------------------------------------------------
// Platform counters
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    // Hand-written declarations: the crate takes no extra dependency for two
    // `kernel32` calls (std already links `kernel32`).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct IoCountersRaw {
        pub read_operation_count: u64,
        pub write_operation_count: u64,
        pub other_operation_count: u64,
        pub read_transfer_count: u64,
        pub write_transfer_count: u64,
        pub other_transfer_count: u64,
    }

    extern "system" {
        pub fn GetCurrentProcess() -> isize;
        pub fn GetProcessIoCounters(process: isize, counters: *mut IoCountersRaw) -> i32;
        pub fn SetPriorityClass(process: isize, priority_class: u32) -> i32;
    }
}

/// Sample this process's I/O counters. `Err` carries the reason the sample is
/// unavailable (non-Windows, or a failed call).
pub fn sample_io() -> Result<IoCounters, String> {
    #[cfg(windows)]
    {
        let mut raw = win::IoCountersRaw::default();
        // SAFETY: the process handle is the immutable pseudo handle of this
        // process and `raw` is a valid, writable `IO_COUNTERS` buffer.
        let ok = unsafe { win::GetProcessIoCounters(win::GetCurrentProcess(), &mut raw) };
        if ok == 0 {
            return Err(format!(
                "GetProcessIoCounters failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(IoCounters {
            read_ops: raw.read_operation_count,
            write_ops: raw.write_operation_count,
            other_ops: raw.other_operation_count,
            read_bytes: raw.read_transfer_count,
            write_bytes: raw.write_transfer_count,
            other_bytes: raw.other_transfer_count,
        })
    }
    #[cfg(not(windows))]
    {
        Err("process I/O counters are only available on Windows".to_string())
    }
}

// ---------------------------------------------------------------------------
// Background mode
// ---------------------------------------------------------------------------

/// `background` block of a CLI report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackgroundReport {
    pub requested: bool,
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type PriorityCall = fn(u32) -> Result<(), String>;

#[derive(Default)]
struct BackgroundHooks {
    begin: Option<PriorityCall>,
    end: Option<PriorityCall>,
}

thread_local! {
    // Test seam: the priority calls a guard makes. Production leaves both
    // `None`, so the real `SetPriorityClass` is used.
    static BACKGROUND_HOOKS: RefCell<BackgroundHooks> = const { RefCell::new(BackgroundHooks { begin: None, end: None }) };
    static BACKGROUND_REPORT: RefCell<Option<BackgroundReport>> = const { RefCell::new(None) };
}

/// Install stub priority calls for the current thread (tests only). Passing
/// `None` restores the real platform call.
pub fn set_background_hooks(begin: Option<PriorityCall>, end: Option<PriorityCall>) {
    BACKGROUND_HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        hooks.begin = begin;
        hooks.end = end;
    });
}

fn set_priority(class: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        // SAFETY: `GetCurrentProcess` returns this process's pseudo handle;
        // `SetPriorityClass` only reads the class value.
        let ok = unsafe { win::SetPriorityClass(win::GetCurrentProcess(), class) };
        if ok == 0 {
            return Err(format!(
                "SetPriorityClass(0x{class:08x}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = class;
        Err("background mode is only available on Windows".to_string())
    }
}

fn call_begin(class: u32) -> Result<(), String> {
    if let Some(hook) = BACKGROUND_HOOKS.with(|hooks| hooks.borrow().begin) {
        return hook(class);
    }
    set_priority(class)
}

fn call_end(class: u32) -> Result<(), String> {
    if let Some(hook) = BACKGROUND_HOOKS.with(|hooks| hooks.borrow().end) {
        return hook(class);
    }
    set_priority(class)
}

/// Enter background mode and keep the paired exit in a `Drop` guard: every
/// return path (success, usage error, panic unwind) leaves the mode again.
pub struct BackgroundGuard {
    report: BackgroundReport,
}

impl BackgroundGuard {
    pub fn report(&self) -> &BackgroundReport {
        &self.report
    }
}

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        if self.report.applied {
            // A failed exit is reported by the begin report already; the
            // process is about to end, so never panic here.
            let _ = call_end(PROCESS_MODE_BACKGROUND_END);
        }
    }
}

/// Request background mode. A failure never aborts the command: it is carried
/// in the report.
pub fn begin_background() -> BackgroundGuard {
    let report = match call_begin(PROCESS_MODE_BACKGROUND_BEGIN) {
        Ok(()) => BackgroundReport {
            requested: true,
            applied: true,
            error: None,
        },
        Err(error) => BackgroundReport {
            requested: true,
            applied: false,
            error: Some(error),
        },
    };
    // The report is installed so every JSON printed by this command carries
    // the `background` block without threading a flag through each report.
    BACKGROUND_REPORT.with(|slot| *slot.borrow_mut() = Some(report.clone()));
    BackgroundGuard { report }
}

/// The installed `background` block, if this command requested the mode.
pub fn background_report() -> Option<BackgroundReport> {
    BACKGROUND_REPORT.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn cost_without_counters_reports_the_reason() {
        let cost = Cost::sample(12, None, None);
        assert_eq!(cost.elapsed_ms, 12);
        assert_eq!(cost.read_bytes, None);
        assert!(cost.cost_unavailable.is_some(), "{cost:?}");
    }

    #[test]
    fn cost_delta_uses_the_named_fields() {
        let start = IoCounters {
            read_ops: 3,
            write_ops: 1,
            other_ops: 0,
            read_bytes: 100,
            write_bytes: 40,
            other_bytes: 0,
        };
        let end = IoCounters {
            read_ops: 9,
            write_ops: 2,
            other_ops: 0,
            read_bytes: 700,
            write_bytes: 60,
            other_bytes: 0,
        };
        let cost = Cost::sample(5, Some(start), Some(end));
        assert_eq!(cost.read_bytes, Some(600));
        assert_eq!(cost.write_bytes, Some(20));
        assert_eq!(cost.read_ops, Some(6));
        assert!(cost.cost_unavailable.is_none());
    }

    #[test]
    fn phases_accumulate_and_are_taken_once() {
        begin_recording();
        {
            let _outer = phase("live_scan");
            record_duration("hash", std::time::Duration::from_millis(3));
            record_duration("hash", std::time::Duration::from_millis(4));
        }
        let cost = finish_recording().expect("recording was active");
        assert!(cost.phases.contains_key("live_scan"));
        assert_eq!(cost.phases["hash"].elapsed_ms, 7);
        assert!(finish_recording().is_none(), "a second finish sees nothing");
    }

    #[test]
    fn phases_are_cheap_when_no_recorder_runs() {
        let _ = finish_recording();
        let _guard = phase("live_scan");
        record_duration("hash", std::time::Duration::from_millis(2));
        assert!(finish_recording().is_none());
    }

    #[test]
    fn counters_accumulate_and_are_taken_once() {
        begin_recording();
        counter("evidence_read.baseline_chunks", 2);
        counter("evidence_read.baseline_chunks", 3);
        counter("watermark_rows.part", 10);
        let cost = finish_recording().expect("recording was active");
        assert_eq!(cost.counters["evidence_read.baseline_chunks"], 5);
        assert_eq!(cost.counters["watermark_rows.part"], 10);
        assert!(finish_recording().is_none());
    }

    #[test]
    fn counters_are_ignored_when_no_recorder_runs() {
        let _ = finish_recording();
        counter("anything", 1);
        assert!(finish_recording().is_none());
    }

    thread_local! {
        static CALLS: RefCell<Vec<(String, u32)>> = const { RefCell::new(Vec::new()) };
        static END_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    fn stub_begin(class: u32) -> Result<(), String> {
        CALLS.with(|calls| calls.borrow_mut().push(("begin".to_string(), class)));
        Ok(())
    }

    fn stub_end(class: u32) -> Result<(), String> {
        CALLS.with(|calls| calls.borrow_mut().push(("end".to_string(), class)));
        Ok(())
    }

    fn reset_calls() {
        CALLS.with(|calls| calls.borrow_mut().clear());
        END_CALLS.with(|count| count.set(0));
    }

    fn calls() -> Vec<(String, u32)> {
        CALLS.with(|calls| calls.borrow().clone())
    }

    #[test]
    fn background_guard_pairs_begin_and_end() {
        reset_calls();
        set_background_hooks(Some(stub_begin), Some(stub_end));
        {
            let guard = begin_background();
            assert!(guard.report().applied);
            assert!(guard.report().requested);
            assert!(background_report().is_some(), "the report is installed");
        }
        set_background_hooks(None, None);
        assert_eq!(
            calls(),
            vec![
                ("begin".to_string(), PROCESS_MODE_BACKGROUND_BEGIN),
                ("end".to_string(), PROCESS_MODE_BACKGROUND_END),
            ]
        );
    }

    #[test]
    fn background_guard_pairs_on_the_error_path_too() {
        reset_calls();
        set_background_hooks(Some(stub_begin), Some(stub_end));
        // A command body that fails still drops the guard before returning.
        fn failing_command() -> Result<(), String> {
            let _guard = begin_background();
            Err("usage".to_string())
        }
        let result = failing_command();
        set_background_hooks(None, None);
        assert!(result.is_err());
        let calls = calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[1].0, "end");
    }

    #[test]
    fn a_failed_begin_is_reported_and_never_ends() {
        reset_calls();
        fn failing_begin(_class: u32) -> Result<(), String> {
            CALLS.with(|calls| calls.borrow_mut().push(("begin".to_string(), 0)));
            END_CALLS.with(|count| count.set(count.get() + 1));
            Err("denied".to_string())
        }
        fn never_end(class: u32) -> Result<(), String> {
            CALLS.with(|calls| calls.borrow_mut().push(("end".to_string(), class)));
            Ok(())
        }
        set_background_hooks(Some(failing_begin), Some(never_end));
        {
            let guard = begin_background();
            assert!(!guard.report().applied);
            assert_eq!(guard.report().error.as_deref(), Some("denied"));
        }
        set_background_hooks(None, None);
        let calls = calls();
        assert_eq!(
            calls.len(),
            1,
            "an unapplied mode is never ended: {calls:?}"
        );
        assert_eq!(calls[0].0, "begin");
    }
}
