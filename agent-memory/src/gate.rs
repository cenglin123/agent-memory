//! §E heavy-job gate: user idle, full-screen app and AC power.
//!
//! The gate decides whether a **heavy** maintenance job (the daily grounding
//! audit, the 7-day SQLite verification) may run now. It never touches the
//! light paths: session-start injection, `memory_recent` / `memory_search` /
//! `memory_read` and the 30-minute incremental index refresh (including the
//! OpenCode tail observation and the rolling reconciliation) run unconditionally,
//! with `--background` lowering their priority only.
//!
//! # Signals (Windows, hand-written `extern "system"`, no extra crate)
//!
//! | signal | call | blocking when |
//! |---|---|---|
//! | user idle | `GetLastInputInfo` + `GetTickCount` | idle < 10 min |
//! | full-screen app | `SHQueryUserNotificationState` | busy / presentation / D3D full screen |
//! | power | `GetSystemPowerStatus` | `ACLineStatus == 0` (on battery) |
//!
//! `GetLastInputInfo` (`user32.dll`) and `SHQueryUserNotificationState`
//! (`shell32.dll`) are **resolved at run time** with `LoadLibraryW` +
//! `GetProcAddress` (both `kernel32.dll`) and cached per process, so the release
//! binary has no GUI DLL in its PE import table. `GetTickCount` and
//! `GetSystemPowerStatus` are `kernel32` exports and stay statically linked.
//!
//! Detection failures (non-Windows, a library or symbol that cannot be
//! resolved, or a call that fails) are **disclosed and do not block**: `None`
//! means "unknown", the decision records `detection.<signal>` and the reason,
//! and the job still runs. A failing probe must never starve maintenance
//! forever, and it must never silently pretend the machine was idle — both are
//! visible in the report.
//!
//! Known limits (documented, not fixable from these APIs): a borderless
//! full-screen game is not always reported by `SHQueryUserNotificationState`,
//! and gamepad input does not reset the keyboard/mouse idle timer, so an idle
//! machine with a controller in use looks idle.
//!
//! # Deferral policy
//!
//! Blocked jobs are deferred to the next round, and run **unconditionally** once
//! the block has lasted [`MAX_DEFER_MS`] (48 h): `first_deferred_at` is kept in
//! the job's gate state file, so a permanently "busy" desktop still gets one
//! grounding/verification every two days.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::archive::now_ms;
use crate::config::atomic_write;

/// Idle threshold of the plan (10 minutes).
pub const IDLE_MIN_MS: u64 = 10 * 60 * 1000;
/// Maximum time a heavy job stays deferred (48 h).
pub const MAX_DEFER_MS: i64 = 48 * 60 * 60 * 1000;
/// Gate state schema (the deferral bookkeeping file).
pub const GATE_STATE_SCHEMA: &str = "agent-memory/gate@1";

/// What the three probes saw. `None` = the probe is unavailable (disclosed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GateObservation {
    /// Milliseconds since the last keyboard/mouse input.
    pub idle_ms: Option<u64>,
    /// True when a full-screen application (or presentation mode) is running.
    pub fullscreen: Option<bool>,
    /// True on AC power, false on battery, None when unknown.
    pub on_ac_power: Option<bool>,
    /// Raw `QUERY_USER_NOTIFICATION_STATE` value, when the call succeeded.
    pub notification_state: Option<i32>,
    /// Which call produced each signal (`GetLastInputInfo`,
    /// `SHQueryUserNotificationState`, `GetSystemPowerStatus`, or
    /// `unavailable: <reason>`).
    pub detection: BTreeMapLabeled,
}

/// Stable, ordered detection labels (`idle` / `fullscreen` / `power`).
pub type BTreeMapLabeled = std::collections::BTreeMap<String, String>;

impl GateObservation {
    fn label(&mut self, signal: &str, method: &str) {
        self.detection.insert(signal.to_string(), method.to_string());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GateAction {
    /// Run now.
    #[serde(rename = "run")]
    Run,
    /// Run now because the block outlasted [`MAX_DEFER_MS`].
    #[serde(rename = "run_forced")]
    ForcedRun,
    /// Defer to the next round.
    #[serde(rename = "defer")]
    Defer,
}

impl GateAction {
    pub fn as_str(self) -> &'static str {
        match self {
            GateAction::Run => "run",
            GateAction::ForcedRun => "run_forced",
            GateAction::Defer => "defer",
        }
    }

    /// True when the caller should execute the job in this round.
    pub fn executes(self) -> bool {
        !matches!(self, GateAction::Defer)
    }
}

/// Policy knobs. `idle_min_ms` and `max_defer_ms` come from the plan; a test can
/// build the same struct with other values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatePolicy {
    pub idle_min_ms: u64,
    pub max_defer_ms: i64,
}

impl Default for GatePolicy {
    fn default() -> Self {
        GatePolicy {
            idle_min_ms: IDLE_MIN_MS,
            max_defer_ms: MAX_DEFER_MS,
        }
    }
}

/// The gate's verdict, with everything the report has to disclose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateDecision {
    pub action: GateAction,
    /// At least one blocking condition was observed.
    pub blocked: bool,
    /// The block outlasted `max_defer_ms` (the action is `ForcedRun`).
    pub forced: bool,
    /// Every blocking reason plus every undisclosed-signal reason, stable order.
    pub reasons: Vec<String>,
    pub idle_ms: Option<u64>,
    pub idle_required_ms: u64,
    pub fullscreen: Option<bool>,
    pub notification_state: Option<i32>,
    pub on_ac_power: Option<bool>,
    /// Consecutive blocked rounds **including** this one (`0` when the job
    /// executes): the value the job's state file holds after the round.
    pub deferrals: u64,
    /// The deferral count this assessment started from.
    pub deferrals_before: u64,
    /// How long the job has been blocked (since its first deferral).
    pub deferred_ms: Option<i64>,
    pub max_defer_ms: i64,
    pub detection: BTreeMapLabeled,
    /// The evidence used to make the decision, never a bare boolean.
    pub assessed_at_ms: i64,
}

/// Decide what a heavy job does this round.
///
/// `deferrals` / `first_deferred_at_ms` are the job's persisted deferral
/// bookkeeping (0 / None on a fresh state); the decision they produce is what
/// [`GateState::record`] writes back.
pub fn assess(
    observation: &GateObservation,
    policy: &GatePolicy,
    deferrals: u64,
    first_deferred_at_ms: Option<i64>,
    now_ms: i64,
) -> GateDecision {
    let mut reasons: Vec<String> = Vec::new();
    let mut blocked = false;
    match observation.idle_ms {
        Some(idle) if idle < policy.idle_min_ms => {
            blocked = true;
            reasons.push(format!(
                "idle_below_threshold: {} ms < {} ms",
                idle, policy.idle_min_ms
            ));
        }
        Some(_) => {}
        None => reasons.push(format!(
            "idle_undisclosed: {}",
            observation
                .detection
                .get("idle")
                .map(String::as_str)
                .unwrap_or("unavailable")
        )),
    }
    match observation.fullscreen {
        Some(true) => {
            blocked = true;
            reasons.push(format!(
                "fullscreen_app: notification_state={}",
                observation
                    .notification_state
                    .map(|state| state.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ));
        }
        Some(false) => {}
        None => reasons.push(format!(
            "fullscreen_undisclosed: {}",
            observation
                .detection
                .get("fullscreen")
                .map(String::as_str)
                .unwrap_or("unavailable")
        )),
    }
    match observation.on_ac_power {
        Some(false) => {
            blocked = true;
            reasons.push("on_battery_power".to_string());
        }
        Some(true) => {}
        None => reasons.push(format!(
            "power_undisclosed: {}",
            observation
                .detection
                .get("power")
                .map(String::as_str)
                .unwrap_or("unavailable")
        )),
    }

    let deferred_ms = first_deferred_at_ms.map(|first| now_ms.saturating_sub(first));
    let forced = blocked
        && deferred_ms
            .map(|elapsed| elapsed >= policy.max_defer_ms)
            .unwrap_or(false);
    let action = if !blocked {
        GateAction::Run
    } else if forced {
        GateAction::ForcedRun
    } else {
        GateAction::Defer
    };
    if forced {
        reasons.push(format!(
            "forced_after_defer: {} ms >= {} ms",
            deferred_ms.unwrap_or_default(),
            policy.max_defer_ms
        ));
    }
    // A deferred round extends the block by one; an executed round ends it.
    let deferrals_after = if action == GateAction::Defer {
        deferrals.saturating_add(1)
    } else {
        0
    };
    GateDecision {
        action,
        blocked,
        forced,
        reasons,
        idle_ms: observation.idle_ms,
        idle_required_ms: policy.idle_min_ms,
        fullscreen: observation.fullscreen,
        notification_state: observation.notification_state,
        on_ac_power: observation.on_ac_power,
        deferrals: deferrals_after,
        deferrals_before: deferrals,
        deferred_ms,
        max_defer_ms: policy.max_defer_ms,
        detection: observation.detection.clone(),
        assessed_at_ms: now_ms,
    }
}

/// Persisted deferral bookkeeping for one job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateState {
    pub schema: String,
    pub job: String,
    #[serde(default)]
    pub deferrals: u64,
    /// RFC3339 instant of the first deferral of the current block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_deferred_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_defer_at: Option<String>,
    /// `run` / `run_forced` / `defer` of the last assessment.
    #[serde(default)]
    pub last_action: String,
    #[serde(default)]
    pub last_assessed_at: String,
    /// Total deferrals since the job last ran.
    #[serde(default)]
    pub deferrals_total: u64,
}

impl GateState {
    pub fn new(job: &str) -> GateState {
        GateState {
            schema: GATE_STATE_SCHEMA.to_string(),
            job: job.to_string(),
            deferrals: 0,
            first_deferred_at: None,
            last_defer_at: None,
            last_action: String::new(),
            last_assessed_at: String::new(),
            deferrals_total: 0,
        }
    }

    /// Load the job's gate state. A missing, unreadable or foreign file is a
    /// fresh state (deferral bookkeeping is reconstructible, never evidence).
    pub fn load(path: &Path, job: &str) -> GateState {
        let fresh = |reason: &str| {
            let mut state = GateState::new(job);
            state.last_action = format!("reset_{reason}");
            state
        };
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return fresh("missing"),
            Err(_) => return fresh("unreadable"),
        };
        match serde_json::from_slice::<GateState>(&raw) {
            Ok(state) if state.schema == GATE_STATE_SCHEMA && state.job == job => state,
            _ => fresh("corrupt"),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let data = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        atomic_write(path, data.as_bytes()).map_err(|error| error.to_string())
    }

    /// Fold one decision into the state: a deferral extends the current block, an
    /// executed round ends it.
    pub fn record(&mut self, decision: &GateDecision, stamp: &str) {
        self.last_action = decision.action.as_str().to_string();
        self.last_assessed_at = stamp.to_string();
        match decision.action {
            GateAction::Defer => {
                self.deferrals = self.deferrals.saturating_add(1);
                self.deferrals_total = self.deferrals_total.saturating_add(1);
                if self.first_deferred_at.is_none() {
                    self.first_deferred_at = Some(stamp.to_string());
                }
                self.last_defer_at = Some(stamp.to_string());
            }
            GateAction::Run | GateAction::ForcedRun => {
                self.deferrals = 0;
                self.first_deferred_at = None;
            }
        }
    }

    /// The start of the current deferral block, in epoch milliseconds.
    pub fn first_deferred_at_ms(&self) -> Option<i64> {
        self.first_deferred_at.as_deref().and_then(parse_ms)
    }
}

/// Parse one RFC3339 instant into epoch milliseconds.
pub fn parse_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|stamp| stamp.timestamp_millis())
}

/// One epoch-millisecond instant as RFC3339 UTC.
pub fn stamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|stamp| stamp.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Platform probes
// ---------------------------------------------------------------------------

/// `QUERY_USER_NOTIFICATION_STATE` values that mean "a full-screen application
/// owns the desktop" (`shellapi.h`).
pub const QUNS_BUSY: i32 = 2;
pub const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
pub const QUNS_PRESENTATION_MODE: i32 = 4;

/// True when a notification state means a full-screen application is running.
pub fn notification_state_is_fullscreen(state: i32) -> bool {
    matches!(
        state,
        QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE
    )
}

/// `LASTINPUTINFO` (`winuser.h`). Plain data: declaring it links nothing.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LastInputInfo {
    pub cb_size: u32,
    pub dw_time: u32,
}

/// `GetLastInputInfo` (`user32.dll`), resolved at run time.
pub type GetLastInputInfoFn = unsafe extern "system" fn(*mut LastInputInfo) -> i32;
/// `SHQueryUserNotificationState` (`shell32.dll`), resolved at run time.
pub type QueryUserNotificationStateFn = unsafe extern "system" fn(*mut i32) -> i32;

/// The two GUI-library calls the gate needs.
///
/// They are deliberately **not** statically imported: the release artifact must
/// have no GUI DLL in its PE import table (`assets/scripts/agent-memory-common.ps1`
/// refuses such a binary), so both are resolved with `LoadLibraryW` +
/// `GetProcAddress` — which live in `kernel32.dll` — on first use and cached for
/// the life of the process. A host without the library, or a DLL that does not
/// export the symbol, yields [`Unavailable`]: the signal is reported as
/// unavailable and disclosed, exactly like a failing call.
#[derive(Clone, Copy)]
pub(crate) struct GuiProbes {
    last_input_info: GetLastInputInfoFn,
    notification_state: QueryUserNotificationStateFn,
}

/// Why a GUI symbol could not be resolved.
///
/// Every variant becomes the `unavailable: …` label of the affected signal, so a
/// downgraded probe is always visible in `detection` / `reasons` and never
/// silently skipped.
///
/// `NotWindows` is constructed only by the `cfg(not(windows))` stubs; on
/// non-Windows hosts without tests the two resolution variants have no
/// constructor left, because there is nothing to resolve there.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// Non-Windows host (all three signals report the same reason).
    #[cfg_attr(windows, allow(dead_code))]
    NotWindows,
    /// `LoadLibraryW` returned null.
    LibraryMissing(&'static str),
    /// The library loaded but does not export the symbol.
    SymbolMissing {
        library: &'static str,
        symbol: &'static str,
    },
}

impl Unavailable {
    pub(crate) fn reason(&self) -> String {
        match self {
            Unavailable::NotWindows => "not Windows".to_string(),
            Unavailable::LibraryMissing(library) => format!("{library} could not be loaded"),
            Unavailable::SymbolMissing { library, symbol } => {
                format!("{symbol} is not exported by {library}")
            }
        }
    }
}

/// Where [`GuiProbes`] come from. The production implementation is the Win32
/// loader in this file; tests inject a double, which is how "the library is
/// missing" and "the symbol is missing" are covered without touching a DLL.
#[cfg(any(windows, test))]
pub(crate) trait SymbolLoader {
    /// `LoadLibraryW`: a module handle, or `None` when the DLL cannot be loaded.
    fn library(&self, library: &'static str) -> Option<*mut std::ffi::c_void>;
    /// `GetProcAddress`: the symbol's address inside `library`.
    fn symbol(
        &self,
        library: *mut std::ffi::c_void,
        symbol: &'static str,
    ) -> Option<*mut std::ffi::c_void>;
}

/// The two GUI DLLs, named once (they appear in the report when missing).
#[cfg(any(windows, test))]
const USER32: &str = "user32.dll";
#[cfg(any(windows, test))]
const SHELL32: &str = "shell32.dll";

/// Resolve both calls through `loader`, in a fixed order (user32 first), so a
/// missing library or symbol always produces the same disclosure.
#[cfg(any(windows, test))]
fn resolve_gui_probes<L: SymbolLoader>(loader: &L) -> Result<GuiProbes, Unavailable> {
    use std::ffi::c_void;
    let user32 = loader
        .library(USER32)
        .ok_or(Unavailable::LibraryMissing(USER32))?;
    let shell32 = loader
        .library(SHELL32)
        .ok_or(Unavailable::LibraryMissing(SHELL32))?;
    // Kept short so the chain stays on one element per line under rustfmt: the
    // two answers name the library and the symbol the disclosure reports.
    let missing = |library, symbol| Unavailable::SymbolMissing { library, symbol };
    let last_input_info = loader
        .symbol(user32, "GetLastInputInfo")
        .ok_or_else(|| missing(USER32, "GetLastInputInfo"))?;
    let notification_state = loader
        .symbol(shell32, "SHQueryUserNotificationState")
        .ok_or_else(|| missing(SHELL32, "SHQueryUserNotificationState"))?;
    // SAFETY: `GetProcAddress` answers with the address of the export that was
    // asked for, and the two signatures below are the documented ones
    // (`BOOL GetLastInputInfo(PLASTINPUTINFO)`, `HRESULT
    // SHQueryUserNotificationState(QUERY_USER_NOTIFICATION_STATE*)`). A loader
    // that answered with an unrelated address would violate `SymbolLoader`.
    Ok(unsafe {
        GuiProbes {
            last_input_info: std::mem::transmute::<*mut c_void, GetLastInputInfoFn>(
                last_input_info,
            ),
            notification_state: std::mem::transmute::<*mut c_void, QueryUserNotificationStateFn>(
                notification_state,
            ),
        }
    })
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use super::SymbolLoader;

    /// `SYSTEM_POWER_STATUS` (`winbase.h`).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct SystemPowerStatus {
        pub ac_line_status: u8,
        pub battery_flag: u8,
        pub battery_life_percent: u8,
        pub system_status_flag: u8,
        pub battery_life_time: u32,
        pub battery_full_life_time: u32,
        pub reserved: u8,
    }

    // SAFETY: every declaration matches the documented Win32 signature; all
    // pointers passed by this module point at writable stack values of the
    // documented C type. All four are `kernel32.dll` exports: the two probes
    // that would drag a GUI DLL into the import table are resolved through
    // `LoadLibraryW` / `GetProcAddress` instead (see `super::GuiProbes`).
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetTickCount() -> u32;
        pub fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
        pub fn LoadLibraryW(name: *const u16) -> *mut c_void;
        pub fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    /// The production [`SymbolLoader`]: `LoadLibraryW` + `GetProcAddress`.
    ///
    /// Loaded modules are never freed: the resolved addresses live in the
    /// process-wide probe cache, and unloading a DLL whose functions are still
    /// cached is how a process crashes later. One load per library per process.
    pub(crate) struct System;

    impl SymbolLoader for System {
        fn library(&self, library: &'static str) -> Option<*mut c_void> {
            let mut wide: Vec<u16> = library.encode_utf16().collect();
            wide.push(0);
            // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the
            // call; `LoadLibraryW` only reads it.
            let module = unsafe { LoadLibraryW(wide.as_ptr()) };
            (!module.is_null()).then_some(module)
        }

        fn symbol(&self, library: *mut c_void, symbol: &'static str) -> Option<*mut c_void> {
            let mut name = symbol.as_bytes().to_vec();
            name.push(0);
            // SAFETY: `name` is a NUL-terminated ASCII export name and `library`
            // is the handle `LoadLibraryW` returned; `GetProcAddress` only reads
            // the name.
            let address = unsafe { GetProcAddress(library, name.as_ptr()) };
            (!address.is_null()).then_some(address)
        }
    }
}

/// The resolved GUI calls of this process.
///
/// `Err` is cached like `Ok`: a host without `shell32.dll` must not pay a
/// `LoadLibraryW` per probe, and a failed lookup is not retried inside one
/// process, so the disclosed label cannot flap between rounds.
#[cfg(windows)]
fn gui_probes() -> Result<GuiProbes, String> {
    static CACHE: std::sync::OnceLock<Result<GuiProbes, Unavailable>> = std::sync::OnceLock::new();
    match CACHE.get_or_init(|| resolve_gui_probes(&win::System)) {
        Ok(probes) => Ok(*probes),
        Err(error) => Err(error.reason()),
    }
}

#[cfg(not(windows))]
fn gui_probes() -> Result<GuiProbes, String> {
    Err(Unavailable::NotWindows.reason())
}

/// `GetTickCount` of this host (`0` on non-Windows, where no probe resolves).
#[cfg(windows)]
fn current_ticks() -> u32 {
    // SAFETY: `GetTickCount` has no arguments and no failure mode.
    unsafe { win::GetTickCount() }
}

#[cfg(not(windows))]
fn current_ticks() -> u32 {
    0
}

/// Idle milliseconds from one resolved `GetLastInputInfo` answer.
fn idle_ms_with(probes: &GuiProbes, ticks: u32) -> Result<u64, String> {
    let mut info = LastInputInfo {
        cb_size: std::mem::size_of::<LastInputInfo>() as u32,
        dw_time: 0,
    };
    // SAFETY: `info` is a valid, writable LASTINPUTINFO with cbSize set, and
    // `last_input_info` is the documented `GetLastInputInfo` (see `GuiProbes`).
    let ok = unsafe { (probes.last_input_info)(&mut info) };
    if ok == 0 {
        return Err("GetLastInputInfo failed".to_string());
    }
    // Both values are 32-bit millisecond counters: wrapping subtraction gives
    // the idle time across the 49.7-day wrap, capped at 49 days.
    Ok(u64::from(ticks.wrapping_sub(info.dw_time)))
}

/// The notification state from one resolved `SHQueryUserNotificationState`.
fn notification_state_with(probes: &GuiProbes) -> Result<i32, String> {
    let mut state: i32 = 0;
    // SAFETY: `state` is a valid, writable QUERY_USER_NOTIFICATION_STATE, and
    // `notification_state` is the documented call (see `GuiProbes`).
    let hr = unsafe { (probes.notification_state)(&mut state) };
    if hr < 0 {
        return Err("SHQueryUserNotificationState failed".to_string());
    }
    Ok(state)
}

fn idle_ms() -> Result<u64, String> {
    let probes = gui_probes()?;
    idle_ms_with(&probes, current_ticks())
}

fn notification_state() -> Result<i32, String> {
    notification_state_with(&gui_probes()?)
}

#[cfg(windows)]
fn ac_power() -> Result<bool, String> {
    let mut status = win::SystemPowerStatus::default();
    // SAFETY: `status` is a valid, writable SYSTEM_POWER_STATUS.
    let ok = unsafe { win::GetSystemPowerStatus(&mut status) };
    if ok == 0 {
        return Err("GetSystemPowerStatus failed".to_string());
    }
    match status.ac_line_status {
        0 => Ok(false),
        1 => Ok(true),
        // 255 = unknown (no battery / undetectable)
        _ => Err("GetSystemPowerStatus reports an unknown AC state".to_string()),
    }
}

#[cfg(not(windows))]
fn ac_power() -> Result<bool, String> {
    Err(Unavailable::NotWindows.reason())
}

/// Environment variable that replaces the three platform probes with fixed
/// values: `{"idle_ms": 700000, "fullscreen": false, "on_ac_power": true}`.
///
/// **Test seam only.** Every report produced under it labels the affected
/// signals `test_override: AM_GATE_TEST_OBSERVATION`, so a probe that did not
/// run can never be mistaken for a real measurement. Unset in production; a
/// malformed value is ignored (the real probes run).
pub const TEST_OBSERVATION_ENV: &str = "AM_GATE_TEST_OBSERVATION";

fn test_observation() -> Option<GateObservation> {
    let raw = std::env::var(TEST_OBSERVATION_ENV).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let mut observation = GateObservation {
        idle_ms: parsed
            .get("idle_ms")
            .and_then(|value| value.as_u64()),
        fullscreen: parsed
            .get("fullscreen")
            .and_then(|value| value.as_bool()),
        on_ac_power: parsed
            .get("on_ac_power")
            .and_then(|value| value.as_bool()),
        notification_state: parsed
            .get("notification_state")
            .and_then(|value| value.as_i64())
            .and_then(|value| i32::try_from(value).ok()),
        ..GateObservation::default()
    };
    for signal in ["idle", "fullscreen", "power"] {
        observation.label(signal, &format!("test_override: {TEST_OBSERVATION_ENV}"));
    }
    Some(observation)
}

/// Read the three signals from the host. Never fails: an unavailable probe is
/// `None` plus a `detection` label naming why.
pub fn observe() -> GateObservation {
    if let Some(observation) = test_observation() {
        return observation;
    }
    let mut observation = GateObservation::default();
    // A signal whose runtime symbol could not be resolved, whose call failed, or
    // that a non-Windows host cannot answer is `None` plus the reason: the job
    // still runs, and the report says why the signal is missing.
    match idle_ms() {
        Ok(idle) => {
            observation.idle_ms = Some(idle);
            observation.label("idle", "GetLastInputInfo");
        }
        Err(reason) => observation.label("idle", &format!("unavailable: {reason}")),
    }
    match notification_state() {
        Ok(state) => {
            observation.notification_state = Some(state);
            observation.fullscreen = Some(notification_state_is_fullscreen(state));
            observation.label("fullscreen", "SHQueryUserNotificationState");
        }
        Err(reason) => observation.label("fullscreen", &format!("unavailable: {reason}")),
    }
    match ac_power() {
        Ok(on_ac) => {
            observation.on_ac_power = Some(on_ac);
            observation.label("power", "GetSystemPowerStatus");
        }
        Err(reason) => observation.label("power", &format!("unavailable: {reason}")),
    }
    observation
}

/// [`observe`] + [`assess`] against one job's persisted state.
pub fn assess_job(
    observation: &GateObservation,
    policy: &GatePolicy,
    state: &GateState,
    now_ms_value: i64,
) -> GateDecision {
    assess(
        observation,
        policy,
        state.deferrals,
        state.first_deferred_at_ms(),
        now_ms_value,
    )
}

/// The gate state path of one job under a log directory.
pub fn state_path(log_dir: &Path, job: &str) -> std::path::PathBuf {
    log_dir.join(format!("gate-{job}.json"))
}

/// Current epoch milliseconds (the crate-wide clock).
pub fn now() -> i64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(idle_ms: Option<u64>, fullscreen: Option<bool>, ac: Option<bool>) -> GateObservation {
        let mut observation = GateObservation {
            idle_ms,
            fullscreen,
            on_ac_power: ac,
            notification_state: fullscreen.map(|value| if value { QUNS_BUSY } else { 5 }),
            ..GateObservation::default()
        };
        observation.label("idle", "test");
        observation.label("fullscreen", "test");
        observation.label("power", "test");
        observation
    }

    #[test]
    fn idle_fullscreen_and_power_combinations() {
        let policy = GatePolicy::default();
        let now = 1_000_000_000_000i64;
        /// (idle, full-screen, AC power, expected action)
        type Case = (Option<u64>, Option<bool>, Option<bool>, GateAction);
        let cases: Vec<Case> = vec![
            // idle (>= 10 min), no full screen, on AC: the only "run" case.
            (Some(IDLE_MIN_MS), Some(false), Some(true), GateAction::Run),
            (Some(IDLE_MIN_MS + 1), Some(false), Some(true), GateAction::Run),
            // too recent input
            (Some(IDLE_MIN_MS - 1), Some(false), Some(true), GateAction::Defer),
            (Some(0), Some(false), Some(true), GateAction::Defer),
            // a full-screen application blocks even when idle and on AC
            (Some(IDLE_MIN_MS), Some(true), Some(true), GateAction::Defer),
            // battery blocks even when idle and no full screen
            (Some(IDLE_MIN_MS), Some(false), Some(false), GateAction::Defer),
            // every blocker at once
            (Some(1), Some(true), Some(false), GateAction::Defer),
            // unknown probes never block (disclosed instead)
            (None, None, None, GateAction::Run),
            (None, Some(false), Some(true), GateAction::Run),
            (Some(IDLE_MIN_MS), None, Some(true), GateAction::Run),
            (Some(IDLE_MIN_MS), Some(false), None, GateAction::Run),
        ];
        for (idle, fullscreen, ac, expected) in cases {
            let decision = assess(&observation(idle, fullscreen, ac), &policy, 0, None, now);
            assert_eq!(
                decision.action, expected,
                "idle={idle:?} fullscreen={fullscreen:?} ac={ac:?}: {decision:?}"
            );
            assert_eq!(decision.blocked, expected == GateAction::Defer, "{decision:?}");
        }
    }

    #[test]
    fn unknown_probes_are_disclosed_never_silent() {
        let policy = GatePolicy::default();
        let decision = assess(&observation(None, None, None), &policy, 0, None, 0);
        assert_eq!(decision.action, GateAction::Run);
        assert!(decision
            .reasons
            .iter()
            .any(|reason| reason.starts_with("idle_undisclosed")));
        assert!(decision
            .reasons
            .iter()
            .any(|reason| reason.starts_with("fullscreen_undisclosed")));
        assert!(decision
            .reasons
            .iter()
            .any(|reason| reason.starts_with("power_undisclosed")));
        assert_eq!(decision.detection.get("idle").map(String::as_str), Some("test"));
    }

    #[test]
    fn a_block_longer_than_the_ceiling_runs_unconditionally() {
        let policy = GatePolicy::default();
        let blocked = observation(Some(0), Some(true), Some(false));
        let first = 1_000_000i64;
        let before = assess(&blocked, &policy, 3, Some(first), first + MAX_DEFER_MS - 1);
        assert_eq!(before.action, GateAction::Defer, "{before:?}");
        assert_eq!(before.deferrals, 4, "the blocked round itself counts");
        assert_eq!(before.deferrals_before, 3);
        let at_ceiling = assess(&blocked, &policy, 3, Some(first), first + MAX_DEFER_MS);
        assert_eq!(at_ceiling.action, GateAction::ForcedRun, "{at_ceiling:?}");
        assert!(at_ceiling.forced);
        assert!(at_ceiling
            .reasons
            .iter()
            .any(|reason| reason.starts_with("forced_after_defer")));
        // An unblocked job is never "forced", even long after the ceiling.
        let free = assess(
            &observation(Some(IDLE_MIN_MS), Some(false), Some(true)),
            &policy,
            9,
            Some(first),
            first + 10 * MAX_DEFER_MS,
        );
        assert_eq!(free.action, GateAction::Run);
        assert!(!free.forced);
    }

    #[test]
    fn state_tracks_one_deferral_block_and_resets_on_execution() {
        let policy = GatePolicy::default();
        let blocked = observation(Some(0), Some(true), Some(false));
        let mut state = GateState::new("grounding");
        let now = 1_000_000i64;
        for round in 0..3 {
            let decision = assess_job(&blocked, &policy, &state, now + round);
            assert_eq!(decision.action, GateAction::Defer);
            let stamp = stamp(now + round);
            state.record(&decision, &stamp);
        }
        assert_eq!(state.deferrals, 3);
        assert_eq!(state.deferrals_total, 3);
        assert_eq!(state.first_deferred_at_ms(), Some(now));
        let forced = assess_job(&blocked, &policy, &state, now + MAX_DEFER_MS);
        assert_eq!(forced.action, GateAction::ForcedRun);
        state.record(&forced, &stamp(now + MAX_DEFER_MS));
        assert_eq!(state.deferrals, 0);
        assert_eq!(state.first_deferred_at, None);
        assert_eq!(state.deferrals_total, 3, "the lifetime count is kept");
    }

    #[test]
    fn gate_state_round_trips_and_tolerates_a_corrupt_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = state_path(temp.path(), "grounding");
        let mut state = GateState::new("grounding");
        state.deferrals = 2;
        state.first_deferred_at = Some(stamp(1_000));
        state.save(&path).expect("save");
        let loaded = GateState::load(&path, "grounding");
        assert_eq!(loaded.deferrals, 2);
        assert_eq!(loaded.first_deferred_at_ms(), Some(1_000));

        std::fs::write(&path, b"{not json").expect("corrupt file");
        let reset = GateState::load(&path, "grounding");
        assert_eq!(reset.deferrals, 0);
        assert_eq!(reset.last_action, "reset_corrupt");

        // A state file of another job never carries over.
        let mut other = GateState::new("verify");
        other.deferrals = 5;
        other.save(&path).expect("save");
        assert_eq!(GateState::load(&path, "grounding").deferrals, 0);
    }

    #[test]
    fn notification_states_name_full_screen_only() {
        assert!(notification_state_is_fullscreen(QUNS_BUSY));
        assert!(notification_state_is_fullscreen(QUNS_RUNNING_D3D_FULL_SCREEN));
        assert!(notification_state_is_fullscreen(QUNS_PRESENTATION_MODE));
        assert!(!notification_state_is_fullscreen(5), "QUNS_ACCEPTS_NOTIFICATIONS");
        assert!(!notification_state_is_fullscreen(7), "QUNS_APP");
    }

    #[test]
    fn observe_never_panics_and_names_every_signal() {
        let observation = observe();
        for signal in ["idle", "fullscreen", "power"] {
            assert!(
                observation.detection.contains_key(signal),
                "signal {signal} must be labelled: {observation:?}"
            );
        }
        if !cfg!(windows) {
            assert!(observation.idle_ms.is_none());
            assert!(observation
                .detection
                .get("idle")
                .unwrap()
                .contains("not Windows"));
        }
    }

    // -----------------------------------------------------------------------
    // P6: the two GUI calls are resolved at run time, never statically linked.
    // -----------------------------------------------------------------------

    /// A [`SymbolLoader`] that answers from fixed name sets. No DLL is touched,
    /// so "the library is missing" and "the symbol is missing" are covered
    /// without a broken system.
    struct FakeLoader {
        libraries: Vec<&'static str>,
        symbols: Vec<&'static str>,
    }

    impl FakeLoader {
        /// Both libraries and both symbols: the resolved path.
        fn complete() -> FakeLoader {
            FakeLoader {
                libraries: vec![USER32, SHELL32],
                symbols: vec!["GetLastInputInfo", "SHQueryUserNotificationState"],
            }
        }

        fn without_library(missing: &'static str) -> FakeLoader {
            let mut loader = FakeLoader::complete();
            loader.libraries.retain(|library| *library != missing);
            loader
        }

        fn without_symbol(missing: &'static str) -> FakeLoader {
            let mut loader = FakeLoader::complete();
            loader.symbols.retain(|symbol| *symbol != missing);
            loader
        }
    }

    /// A non-null module handle the fake loader hands out (`LoadLibraryW`'s
    /// `HMODULE`); [`FakeLoader::symbol`] ignores it.
    const MODULE_HANDLE: usize = 0x1000;

    unsafe extern "system" fn fake_last_input_info(info: *mut LastInputInfo) -> i32 {
        if info.is_null() {
            return 0;
        }
        // SAFETY: the caller passes a writable LASTINPUTINFO.
        unsafe { (*info).dw_time = 4_242 };
        1
    }

    unsafe extern "system" fn fake_query_notification_state(state: *mut i32) -> i32 {
        if state.is_null() {
            return -1;
        }
        // SAFETY: the caller passes a writable QUERY_USER_NOTIFICATION_STATE.
        unsafe { *state = QUNS_PRESENTATION_MODE };
        0
    }

    impl SymbolLoader for FakeLoader {
        fn library(&self, library: &'static str) -> Option<*mut std::ffi::c_void> {
            self.libraries
                .contains(&library)
                .then_some(MODULE_HANDLE as *mut std::ffi::c_void)
        }

        fn symbol(
            &self,
            _library: *mut std::ffi::c_void,
            symbol: &'static str,
        ) -> Option<*mut std::ffi::c_void> {
            if !self.symbols.contains(&symbol) {
                return None;
            }
            let address: *mut std::ffi::c_void = match symbol {
                "GetLastInputInfo" => fake_last_input_info as GetLastInputInfoFn as *mut _,
                _ => fake_query_notification_state as QueryUserNotificationStateFn as *mut _,
            };
            Some(address)
        }
    }

    #[test]
    fn resolved_symbols_are_the_ones_the_probes_then_call() {
        let probes =
            resolve_gui_probes(&FakeLoader::complete()).expect("both libraries and symbols");
        // "Resolved" only means something if the addresses are the calls the
        // probes make: the stand-ins answer with fixed values.
        assert_eq!(idle_ms_with(&probes, 5_000), Ok(5_000 - 4_242));
        assert_eq!(notification_state_with(&probes), Ok(QUNS_PRESENTATION_MODE));
        assert!(notification_state_is_fullscreen(QUNS_PRESENTATION_MODE));
    }

    /// The report path of an unresolvable signal: `null` plus an
    /// `unavailable: …` label, and the job still runs.
    fn assert_disclosed_never_blocking(label: &str) {
        let mut observation = GateObservation::default();
        for signal in ["idle", "fullscreen", "power"] {
            observation.label(signal, label);
        }
        let decision = assess(&observation, &GatePolicy::default(), 0, None, 0);
        assert_eq!(decision.action, GateAction::Run, "{decision:?}");
        assert!(
            !decision.blocked,
            "an unavailable probe never blocks: {decision:?}"
        );
        for signal in ["idle", "fullscreen", "power"] {
            assert_eq!(
                decision.detection.get(signal).map(String::as_str),
                Some(label)
            );
            let prefix = format!("{signal}_undisclosed");
            assert!(
                decision
                    .reasons
                    .iter()
                    .any(|reason| reason.starts_with(&prefix)),
                "{signal} must be disclosed: {decision:?}"
            );
        }
    }

    /// `Ok` carries resolved function pointers, which have no `Debug`: match
    /// instead of `expect_err`.
    fn resolution_error(loader: &FakeLoader) -> Unavailable {
        match resolve_gui_probes(loader) {
            Ok(_) => panic!("the symbols are missing, resolution must fail"),
            Err(error) => error,
        }
    }

    #[test]
    fn a_missing_gui_library_is_disclosed_and_never_fatal() {
        for missing in [USER32, SHELL32] {
            let error = resolution_error(&FakeLoader::without_library(missing));
            assert_eq!(error, Unavailable::LibraryMissing(missing));
            let label = format!("unavailable: {}", error.reason());
            assert!(label.contains(missing), "{label}");
            assert_disclosed_never_blocking(&label);
        }
    }

    #[test]
    fn a_missing_gui_symbol_is_disclosed_and_never_fatal() {
        for (missing, library) in [
            ("GetLastInputInfo", USER32),
            ("SHQueryUserNotificationState", SHELL32),
        ] {
            let error = resolution_error(&FakeLoader::without_symbol(missing));
            assert_eq!(
                error,
                Unavailable::SymbolMissing {
                    library,
                    symbol: missing
                }
            );
            let label = format!("unavailable: {}", error.reason());
            assert!(
                label.contains(missing) && label.contains(library),
                "{label}"
            );
            assert_disclosed_never_blocking(&label);
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_system_loader_resolves_both_gui_calls() {
        // With no static GUI import this is the only proof the gate can still
        // read the two signals on a real host: both DLLs load and export the
        // two documented symbols. A call that then fails is disclosed, not
        // fatal, so only the resolution is asserted here.
        let probes = resolve_gui_probes(&win::System).expect("user32/shell32 export both calls");
        let _ = notification_state_with(&probes);
        let _ = idle_ms_with(&probes, current_ticks());
    }
}
