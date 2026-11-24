//! implementation-005 spec 5: the §E heavy-job gate.
//!
//! Two shapes are pinned here:
//!
//! * the `gate` command the wrapper calls — deterministic via the disclosed test
//!   seam (`AM_GATE_TEST_OBSERVATION`), covering the idle / full-screen / power
//!   combinations and the 48-hour forced run;
//! * the automatic 7-day SQLite verification — a blocked round really skips the
//!   full-table pass (it is not silently "verified"), and the next allowed round
//!   runs it.
//!
//! The light paths are not gated by construction (they never call this module);
//! `capture-all`, `index` and the readback commands keep running while a heavy
//! job is deferred.

use std::path::Path;
use std::process::Command;

use agent_memory::archive::Archive;
use agent_memory::capture::{capture_source, capture_source_with_options, CaptureOptions, CaptureReport};
use agent_memory::gate;
use serde_json::{json, Value};

mod support;

use support::TestEnv;

const DOMAIN: &str = "default";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Busy desktop: recent input, a full-screen application, on battery.
const BUSY: &str = r#"{"idle_ms": 5000, "fullscreen": true, "on_ac_power": false}"#;
/// Free desktop: 20 minutes idle, no full-screen application, on AC power.
const FREE: &str = r#"{"idle_ms": 1200000, "fullscreen": false, "on_ac_power": true}"#;

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
}

fn run_gate(args: &[&str], observation: &str) -> Value {
    let output = Command::new(exe())
        .args(args)
        .env(gate::TEST_OBSERVATION_ENV, observation)
        .output()
        .expect("gate command runs");
    assert!(
        output.status.success(),
        "gate failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("gate JSON")
}

// ---------------------------------------------------------------------------
// 1. The gate command: probe combinations, deferrals, the 48-hour ceiling.
// ---------------------------------------------------------------------------

#[test]
fn gate_command_reports_every_signal_and_its_detection_method() {
    let temp = support::stable_tempdir();
    let state = temp.path().join("gate-grounding.json");
    let state_arg = state.to_string_lossy().to_string();

    let free = run_gate(
        &["gate", "--job", "grounding", "--state", &state_arg],
        FREE,
    );
    assert_eq!(free["action"], "run");
    assert_eq!(free["blocked"], false);
    assert_eq!(free["deferrals"], 0);
    // The report names the method behind every signal (the seam labels itself).
    for signal in ["idle", "fullscreen", "power"] {
        assert!(
            free["detection"][signal].is_string(),
            "signal {signal} must name its detection: {free}"
        );
    }
    assert!(
        free["detection"]["idle"]
            .as_str()
            .unwrap_or_default()
            .contains("test_override"),
        "the seam is disclosed, never passed off as a real probe: {free}"
    );
    assert_eq!(free["idleRequiredMs"], 600000);
    assert_eq!(free["maxDeferMs"], 172800000);

    // Busy: deferred, counted, and the reasons name each blocker.
    let busy = run_gate(
        &["gate", "--job", "grounding", "--state", &state_arg],
        BUSY,
    );
    assert_eq!(busy["action"], "defer");
    assert_eq!(busy["blocked"], true);
    assert_eq!(busy["deferrals"], 1);
    let reasons = busy["reasons"].to_string();
    assert!(reasons.contains("idle_below_threshold"), "{busy}");
    assert!(reasons.contains("fullscreen_app"), "{busy}");
    assert!(reasons.contains("on_battery_power"), "{busy}");
    // The state file carries the deferral block for the next round.
    let deferred_at = busy["deferredMs"].clone();
    assert!(deferred_at.is_null() || deferred_at.as_i64().is_some());

    let again = run_gate(
        &["gate", "--job", "grounding", "--state", &state_arg],
        BUSY,
    );
    assert_eq!(again["deferrals"], 2, "each blocked round counts once");
}

#[test]
fn a_block_longer_than_48_hours_runs_unconditionally() {
    let temp = support::stable_tempdir();
    let state = temp.path().join("gate-grounding.json");
    let state_arg = state.to_string_lossy().to_string();
    let busy = run_gate(
        &["gate", "--job", "grounding", "--state", &state_arg],
        BUSY,
    );
    assert_eq!(busy["action"], "defer");

    // Age the block past the ceiling, as three days of blocked rounds would.
    let mut value: Value =
        serde_json::from_slice(&std::fs::read(&state).expect("state file")).expect("state JSON");
    let old = gate::stamp(gate::now() - 49 * 60 * 60 * 1000);
    value["firstDeferredAt"] = json!(old);
    std::fs::write(&state, serde_json::to_vec_pretty(&value).unwrap()).expect("rewrite state");

    let forced = run_gate(
        &["gate", "--job", "grounding", "--state", &state_arg],
        BUSY,
    );
    assert_eq!(forced["action"], "run_forced", "{forced}");
    assert_eq!(forced["forced"], true);
    assert!(forced["reasons"].to_string().contains("forced_after_defer"));
    // The block is closed again, so the next blocked round starts over.
    let after: Value =
        serde_json::from_slice(&std::fs::read(&state).expect("state file")).expect("state JSON");
    assert_eq!(after["deferrals"], 0);
    assert!(after["firstDeferredAt"].is_null(), "{after}");
}

// ---------------------------------------------------------------------------
// 2. The automatic 7-day verification is gated; an explicit --verify is not.
// ---------------------------------------------------------------------------

fn age_last_verify(archive: &Archive, days: i64) -> i64 {
    let mut state = archive.load_sources().expect("sources");
    let key = state
        .sources
        .keys()
        .next()
        .cloned()
        .expect("one synthetic source");
    let sqlite = state
        .sources
        .get_mut(&key)
        .and_then(|entry| entry.sqlite.as_mut())
        .expect("sqlite state");
    let aged = gate::now() - days * DAY_MS;
    sqlite.last_verify_at_ms = aged;
    archive.save_sources(&state).expect("save sources");
    aged
}

fn last_verify(archive: &Archive) -> i64 {
    let state = archive.load_sources().expect("sources");
    state
        .sources
        .values()
        .find_map(|entry| entry.sqlite.as_ref().map(|sqlite| sqlite.last_verify_at_ms))
        .expect("sqlite state")
}

/// `AM_GATE_TEST_OBSERVATION` is process-global: two tests that install the seam
/// must not interleave their set/remove pairs (Rust runs the tests of one binary
/// on parallel threads), so every capture that uses the seam holds this lock for
/// the whole capture.
static OBSERVATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn capture_with(archive: &Archive, db: &Path, observation: &str, options: CaptureOptions) -> CaptureReport {
    let _guard = OBSERVATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var(gate::TEST_OBSERVATION_ENV, observation);
    let report = capture_source_with_options(archive, db, "opencode", DOMAIN, None, options);
    std::env::remove_var(gate::TEST_OBSERVATION_ENV);
    report.expect("capture runs")
}

fn gate_file(archive: &Archive) -> std::path::PathBuf {
    let mut entries: Vec<_> = std::fs::read_dir(archive.root().join("state").join("sqlite"))
        .expect("sqlite state dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(".gate-verify.json"))
                .unwrap_or(false)
        })
        .collect();
    entries.pop().expect("the verification gate state file exists")
}

#[test]
fn a_blocked_verification_round_defers_and_the_next_free_round_runs_it() {
    let env = TestEnv::new();
    let root = env.root("gate-verify");
    Archive::init(&root).expect("init runtime root");
    let archive = Archive::open(&root).expect("archive opens");
    let fixture = support::build_v1(&env);

    // Baseline: a fresh baseline counts as verified.
    let baseline = capture_source(&archive, &fixture.db, "opencode", DOMAIN, None)
        .expect("baseline capture");
    assert!(baseline.ok, "{baseline:?}");

    // Eight days later the 7-day cycle is due. A busy desktop defers it.
    let aged = age_last_verify(&archive, 8);
    let deferred = capture_with(&archive, &fixture.db, BUSY, CaptureOptions::default());
    assert!(deferred.ok, "{deferred:?}");
    let info = deferred.sqlite.as_ref().expect("sqlite info");
    let verdict = info
        .verify_gate
        .as_ref()
        .expect("the automatic cycle publishes its gate verdict");
    assert_eq!(verdict.action, gate::GateAction::Defer, "{verdict:?}");
    assert!(!info.verify, "a deferred round does not run the full-table pass");
    assert_eq!(
        last_verify(&archive),
        aged,
        "a deferred verification leaves the timestamp alone (it never happened)"
    );
    let state: Value =
        serde_json::from_slice(&std::fs::read(gate_file(&archive)).expect("gate file"))
            .expect("gate state JSON");
    assert_eq!(state["deferrals"], 1, "{state}");

    // The next free round runs it and advances the timestamp.
    let ran = capture_with(&archive, &fixture.db, FREE, CaptureOptions::default());
    assert!(ran.ok, "{ran:?}");
    let info = ran.sqlite.as_ref().expect("sqlite info");
    let verdict = info.verify_gate.as_ref().expect("gate verdict");
    assert_eq!(verdict.action, gate::GateAction::Run, "{verdict:?}");
    assert!(info.verify, "an allowed round runs the verification");
    assert!(
        last_verify(&archive) > aged,
        "the verification timestamp advances"
    );
    let state: Value =
        serde_json::from_slice(&std::fs::read(gate_file(&archive)).expect("gate file"))
            .expect("gate state JSON");
    assert_eq!(state["deferrals"], 0, "an executed round closes the block");
}

#[test]
fn an_explicit_verify_never_waits_for_the_gate() {
    let env = TestEnv::new();
    let root = env.root("gate-explicit");
    Archive::init(&root).expect("init runtime root");
    let archive = Archive::open(&root).expect("archive opens");
    let fixture = support::build_v1(&env);
    capture_source(&archive, &fixture.db, "opencode", DOMAIN, None).expect("baseline capture");
    age_last_verify(&archive, 8);

    // An operator asking for --verify gets it, busy desktop or not.
    let report = capture_with(
        &archive,
        &fixture.db,
        BUSY,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().expect("sqlite info");
    assert!(info.verify, "an explicit --verify runs: {info:?}");
    assert!(
        info.verify_gate.is_none(),
        "an explicit --verify is not gated and says so: {info:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Gate bookkeeping failure is disclosed, never silent.
// ---------------------------------------------------------------------------

#[test]
fn an_unwritable_gate_state_file_is_disclosed() {
    let env = TestEnv::new();
    let root = env.root("gate-unwritable");
    Archive::init(&root).expect("init runtime root");
    let archive = Archive::open(&root).expect("archive opens");
    let fixture = support::build_v1(&env);
    capture_source(&archive, &fixture.db, "opencode", DOMAIN, None).expect("baseline capture");
    age_last_verify(&archive, 8);

    // The first blocked periodic round writes the deferral block.
    let deferred = capture_with(&archive, &fixture.db, BUSY, CaptureOptions::default());
    assert!(deferred.ok, "{deferred:?}");
    let gate_path = gate_file(&archive);

    // Make the bookkeeping unwritable: a directory where its file belongs.
    std::fs::remove_file(&gate_path).expect("remove the gate state file");
    std::fs::create_dir(&gate_path).expect("block the gate state path");
    let blocked = capture_with(&archive, &fixture.db, BUSY, CaptureOptions::default());
    assert!(
        blocked.ok,
        "a capture never fails because its gate bookkeeping does: {blocked:?}"
    );
    let verdict = blocked
        .sqlite
        .as_ref()
        .and_then(|info| info.verify_gate.as_ref())
        .expect("the blocked round publishes its gate verdict");
    assert!(
        verdict
            .reasons
            .iter()
            .any(|reason| reason.contains("gate_state_unwritable")),
        "an unwritable state file is a disclosed reason: {verdict:?}"
    );
    assert!(
        verdict
            .detection
            .get("gate_state")
            .map(|label| label.contains("write_failed"))
            .unwrap_or(false),
        "the detection labels name the failed bookkeeping write: {verdict:?}"
    );
}
