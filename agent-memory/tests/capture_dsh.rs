//! DSH archive capture (`capture-all` source kind `dsh`): logical-line evidence,
//! incremental append, torn/corrupt carriers, recapture and generation switch,
//! `--plan`, archive readback, and the archive fallback of the live index.
//!
//! Every fixture is synthetic and lives under a per-test temporary root. The
//! carriers are real Zstandard frame streams built here; no test points at a
//! real DSH, Claude, Codex or OpenCode directory or at a real Archive.

mod support;

use agent_memory::archive::{hex_sha256, Archive};
use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::codes;
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::read::{memory_read, ReadRequest};
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

use support::TestEnv;

const SESSION: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT: &str = "--C-Users-x-Documents-GitHub--";
const SENTINEL: &str = "dshcapturesentinel";

fn ts(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("timestamp")
        .timestamp_millis()
}

fn header(version: u32, id: &str) -> String {
    json!({
        "type": "session",
        "version": version,
        "id": id,
        "createdAt": ts("2026-11-07T08:00:00Z"),
        "cwd": "C:\\Users\\x\\Documents\\GitHub",
    })
    .to_string()
}

fn event(kind: &str, seq: u64, data: Value) -> String {
    json!({
        "type": kind,
        "seq": seq,
        "time": ts("2026-11-07T09:00:00Z") + seq as i64 * 1_000,
        "data": data,
    })
    .to_string()
}

fn user(text: &str) -> Value {
    json!({
        "id": "m-user",
        "role": "user",
        "source": {"kind": "user"},
        "content": [{"type": "text", "text": text}]
    })
}

fn assistant(text: &str) -> Value {
    json!({
        "message": {
            "id": "m-assistant",
            "role": "assistant",
            "source": {"kind": "model"},
            "content": [{"type": "text", "text": text}]
        }
    })
}

/// One compressed frame holding the given newline-terminated lines.
fn frame_of(lines: &[String]) -> Vec<u8> {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    zstd::stream::encode_all(text.as_bytes(), 3).expect("zstd frame")
}

fn write_frames(path: &Path, frames: &[Vec<u8>]) {
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend_from_slice(frame);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("session dir");
    }
    fs::write(path, bytes).expect("write dsh carrier");
}

fn append_frame(path: &Path, lines: &[String]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(path).expect("open carrier");
    file.write_all(&frame_of(lines)).expect("append frame");
}

fn session_dir(root: &Path) -> PathBuf {
    root.join(PROJECT).join(format!("session-{SESSION}"))
}

struct World {
    env: TestEnv,
    archive_root: PathBuf,
    cache_root: PathBuf,
    dsh_root: PathBuf,
}

fn world(name: &str) -> World {
    let env = TestEnv::new();
    let archive_root = env.root(name);
    Archive::init(&archive_root).expect("init runtime root");
    let cache_root = env.root("cache");
    let dsh_root = env.root("dsh-sessions");
    World {
        env,
        archive_root,
        cache_root,
        dsh_root,
    }
}

impl World {
    fn config(&self, archive_enabled: bool) -> PathBuf {
        let mut sources = vec![json!({
            "source_id": "dsh",
            "kind": "dsh",
            "root": self.dsh_root.to_string_lossy(),
            "enabled": true,
        })];
        let mut order = vec!["dsh"];
        sources.push(json!({
            "source_id": "archive",
            "kind": "archive",
            "root": self.archive_root.to_string_lossy(),
            "enabled": archive_enabled,
        }));
        if archive_enabled {
            order.push("archive");
        }
        self.env.write_json(
            "recall.json",
            &json!({
                "version": 2,
                "sources": sources,
                "source_order": order,
                "interpreter": {"kb": {"command": "C:/python/python.exe"}},
                "domain": "default",
                "telemetry": {"enabled": false},
                "cache_root": self.cache_root.to_string_lossy(),
            }),
        )
    }

    fn capture(&self, config: &Path) -> Value {
        let (report, code) = self.capture_raw(config);
        assert_eq!(code, 0, "capture-all failed: {report}");
        report
    }

    fn capture_raw(&self, config: &Path) -> (Value, u8) {
        let parsed = load_config(config).expect("config loads");
        let options = agent_memory::capture_all::CaptureAllOptions {
            domain: "default".to_string(),
            ..agent_memory::capture_all::CaptureAllOptions::default()
        };
        let (report, code) =
            agent_memory::capture_all::capture_all(&parsed, &options).expect("capture-all runs");
        (serde_json::to_value(report).expect("report json"), code)
    }

    fn plan(&self, config: &Path) -> Value {
        let parsed = load_config(config).expect("config loads");
        let options = agent_memory::capture_all::CaptureAllOptions {
            plan: true,
            domain: "default".to_string(),
            ..agent_memory::capture_all::CaptureAllOptions::default()
        };
        let (report, code) =
            agent_memory::capture_all::capture_all(&parsed, &options).expect("plan runs");
        assert_eq!(code, 0, "plan failed: {:?}", report);
        serde_json::to_value(report).expect("report json")
    }

    fn archive(&self) -> Archive {
        Archive::open(&self.archive_root).expect("open archive")
    }

    fn manifest_values(&self) -> Vec<Value> {
        let archive = self.archive();
        archive
            .manifests()
            .expect("manifests")
            .iter()
            .map(|manifest| serde_json::to_value(manifest).expect("manifest json"))
            .collect()
    }

    fn source_state(&self) -> Value {
        let archive = self.archive();
        let state = archive.load_sources().expect("sources state");
        serde_json::to_value(state).expect("state json")
    }

    fn ledger(&self) -> Value {
        let path = self.archive_root.join("state").join("capture-all.json");
        serde_json::from_str(&fs::read_to_string(path).expect("ledger")).expect("ledger json")
    }

    fn index(&self, config: &Path) -> Vec<Value> {
        let parsed = load_config(config).expect("config loads");
        index_all(&parsed, None, false)
            .expect("index runs")
            .iter()
            .map(|report| serde_json::to_value(report).expect("report json"))
            .collect()
    }

    fn index_report<'a>(&self, reports: &'a [Value], source_id: &str) -> &'a Value {
        reports
            .iter()
            .find(|report| report["source_id"] == json!(source_id))
            .unwrap_or_else(|| panic!("source {source_id} missing: {reports:?}"))
    }

    fn search(&self, config: &Path, request: Value) -> Value {
        let parsed = load_config(config).expect("config loads");
        let request = SearchRequest::parse(&request).expect("search parses");
        let response = memory_search(&parsed, &request);
        let value = serde_json::to_value(&response).expect("response json");
        assert_eq!(response.exit_code, 0, "search failed: {value}");
        value
    }

    fn read(&self, config: &Path, reference: &Value) -> Value {
        let parsed = load_config(config).expect("config loads");
        let request =
            ReadRequest::parse(&json!({"ref": reference, "max_bytes": 65536})).expect("read parses");
        let response = memory_read(&parsed, &request);
        serde_json::to_value(&response).expect("response json")
    }

    fn read_request(&self, config: &Path, request: Value) -> agent_memory::recall::read::ReadResponse {
        let parsed = load_config(config).expect("config loads");
        let request = ReadRequest::parse(&request).expect("read parses");
        memory_read(&parsed, &request)
    }
}

fn source<'a>(report: &'a Value, source_id: &str) -> &'a Value {
    report["sources"]
        .as_array()
        .expect("sources array")
        .iter()
        .find(|entry| entry["source_id"] == json!(source_id))
        .unwrap_or_else(|| panic!("source {source_id} missing: {report}"))
}

fn hit_with_needle<'a>(response: &'a Value, needle: &str) -> &'a Value {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|hit| hit["snippet"].as_str().unwrap_or_default().contains(needle))
        .unwrap_or_else(|| panic!("no hit contains {needle}: {response}"))
}

/// Deterministic tree fingerprint: prove `--plan` writes nothing at all.
fn tree_hash(root: &Path) -> u64 {
    let mut paths: Vec<PathBuf> = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    walk(root, &mut paths);
    paths.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for path in paths {
        let rel = path.strip_prefix(root).expect("relative").to_string_lossy();
        for byte in rel.as_bytes() {
            hash = (hash ^ *byte as u64).wrapping_mul(0x1000_0000_01b3);
        }
        hash = (hash ^ 0u64).wrapping_mul(0x1000_0000_01b3);
        for byte in fs::read(&path).expect("read for hash") {
            hash = (hash ^ byte as u64).wrapping_mul(0x1000_0000_01b3);
        }
    }
    hash
}

fn base_lines(text: &str) -> Vec<String> {
    vec![
        header(4, SESSION),
        event("user/message", 1, user(&format!("{text} user question"))),
        event("assistant/message", 2, assistant(&format!("{text} assistant answer"))),
    ]
}

// ---------------------------------------------------------------------------
// 1. First capture: logical lines, never the compressed carrier.
// ---------------------------------------------------------------------------

#[test]
fn first_capture_archives_logical_lines_and_never_the_carrier() {
    let world = world("dsh-cap-first");
    let dir = session_dir(&world.dsh_root);
    let carrier = dir.join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    write_frames(&carrier, &[frame_of(&lines[..2]), frame_of(&lines[2..])]);
    let logical_len: u64 = lines.iter().map(|line| line.len() as u64 + 1).sum();

    let config = world.config(false);
    let report = world.capture(&config);
    assert_eq!(report["ok"], true);
    let dsh = source(&report, "dsh");
    assert_eq!(dsh["provider"], "dsh");
    assert_eq!(dsh["files_total"], 1);
    assert_eq!(dsh["files_captured"], 1);
    assert_eq!(dsh["records_added"], 3);
    assert_eq!(dsh["dsh"]["sessions_total"], 1);
    assert_eq!(dsh["dsh"]["sessions_unsupported"], 0);
    assert!(
        !report.to_string().contains(SENTINEL),
        "the driver report must never carry session bodies"
    );

    let manifests = world.manifest_values();
    assert_eq!(manifests.len(), 1);
    let manifest = &manifests[0];
    assert_eq!(manifest["provider"], "dsh");
    assert_eq!(manifest["source"]["kind"], "dsh_jsonl");
    assert_eq!(manifest["source"]["generation"], 1);
    assert_eq!(manifest["source"]["range_start"], 0);
    assert_eq!(
        manifest["source"]["carrier"],
        json!(format!("{PROJECT}/session-{SESSION}/session.v4.jsonl.zstd"))
    );
    assert_eq!(manifest["source"]["path"], json!(dir.to_string_lossy()));
    assert_eq!(manifest["records"].as_array().expect("records").len(), 3);
    assert_eq!(manifest["objects"][0]["bytes"], json!(logical_len));
    // The archived object is the decompressed stream, not the zstd carrier.
    assert_eq!(
        manifest["objects"][0]["bytes"],
        json!(manifest["objects"][0]["bytes"].as_u64().unwrap())
    );
    assert!(file_len(&carrier) < logical_len, "compressed carrier is smaller");

    // The checkpoint is the carrier path + committed lines + last-line digest.
    let state = world.source_state();
    let entry = state["sources"]
        .as_object()
        .expect("sources")
        .iter()
        .find(|(_, value)| value["provider"] == json!("dsh"))
        .map(|(_, value)| value)
        .expect("dsh source state");
    assert_eq!(
        entry["dsh"]["rel_path"],
        json!(format!("{PROJECT}/session-{SESSION}/session.v4.jsonl.zstd"))
    );
    assert_eq!(entry["dsh"]["lines"], 3);
    assert_eq!(
        entry["dsh"]["last_line_sha256"],
        json!(hex_sha256(lines[2].as_bytes()))
    );
    assert_eq!(entry["watermark"]["line"], 3);
    assert_eq!(entry["watermark"]["offset"], json!(logical_len));

    let ledger = world.ledger();
    let files = ledger["sources"]["dsh"]["files"].as_object().expect("files");
    assert_eq!(files.len(), 1);
    let ledger_entry = files.values().next().expect("entry");
    assert_eq!(ledger_entry["dsh_lines"], 3);
    assert_eq!(
        ledger_entry["dsh_last_line_sha256"],
        json!(hex_sha256(lines[2].as_bytes()))
    );
    assert!(ledger_entry.get("dsh_previous").is_none());
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).expect("metadata").len()
}

// ---------------------------------------------------------------------------
// 2. Append frame continues from the checkpoint; an unchanged run is skipped.
// ---------------------------------------------------------------------------

#[test]
fn append_frame_continues_from_the_checkpoint_and_unchanged_is_skipped() {
    let world = world("dsh-cap-append");
    let dir = session_dir(&world.dsh_root);
    let carrier = dir.join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    write_frames(&carrier, &[frame_of(&lines)]);
    let config = world.config(false);
    let first = world.capture(&config);
    assert_eq!(source(&first, "dsh")["records_added"], 3);
    let logical_first: u64 = lines.iter().map(|line| line.len() as u64 + 1).sum();

    let extra = event("user/message", 3, user(&format!("{SENTINEL} appended")));
    append_frame(&carrier, &[extra.clone()]);
    let second = world.capture(&config);
    let dsh = source(&second, "dsh");
    assert_eq!(dsh["files_captured"], 1);
    assert_eq!(dsh["records_added"], 1);
    assert_eq!(dsh["dsh"]["partial_tail"], 0);

    let manifests = world.manifest_values();
    assert_eq!(manifests.len(), 2);
    let appended = manifests
        .iter()
        .find(|manifest| manifest["source"]["range_start"] == json!(logical_first))
        .expect("append manifest");
    assert_eq!(appended["source"]["generation"], 1);
    assert_eq!(appended["source"]["capture_reason"], "append");
    assert_eq!(appended["records"][0]["source_offset"], json!(logical_first));
    assert_eq!(appended["records"][0]["source_line"], 4);

    let state = world.source_state();
    let entry = state["sources"]
        .as_object()
        .expect("sources")
        .values()
        .find(|value| value["provider"] == json!("dsh"))
        .expect("dsh state");
    assert_eq!(entry["watermark"]["records"], 4);
    assert_eq!(entry["watermark"]["line"], 4);
    assert_eq!(entry["watermark"]["generation"], 1);

    let third = world.capture(&config);
    let dsh = source(&third, "dsh");
    assert_eq!(dsh["files_skipped_unchanged"], 1);
    assert_eq!(dsh["files_captured"], 0);
    assert_eq!(world.manifest_values().len(), 2);
}

// ---------------------------------------------------------------------------
// 3. A torn trailing frame contributes no line and is not archived.
// ---------------------------------------------------------------------------

#[test]
fn torn_tail_is_held_back_until_the_frame_completes() {
    let world = world("dsh-cap-torn");
    let dir = session_dir(&world.dsh_root);
    let carrier = dir.join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    let last = event("user/message", 3, user(&format!("{SENTINEL} torn tail")));
    write_frames(&carrier, &[frame_of(&lines[..2]), frame_of(&lines[2..])]);
    let last_frame = frame_of(&[last.clone()]);
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&carrier).expect("open");
        file.write_all(&last_frame[..last_frame.len() - 6]).expect("torn frame");
    }
    let config = world.config(false);
    let first = world.capture(&config);
    let dsh = source(&first, "dsh");
    assert_eq!(dsh["files_captured"], 1);
    assert_eq!(dsh["records_added"], 3);
    assert_eq!(dsh["dsh"]["partial_tail"], 1);
    let manifests = world.manifest_values();
    assert_eq!(manifests[0]["records"].as_array().expect("records").len(), 3);

    // Finish the frame: the held-back line is the only thing appended.
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&carrier).expect("open");
        file.write_all(&last_frame[last_frame.len() - 6..]).expect("complete frame");
    }
    let second = world.capture(&config);
    let dsh = source(&second, "dsh");
    assert_eq!(dsh["records_added"], 1);
    assert_eq!(dsh["dsh"]["partial_tail"], 0);
    let manifests = world.manifest_values();
    assert_eq!(manifests.len(), 2);
    assert_eq!(
        manifests
            .iter()
            .find(|manifest| manifest["source"]["capture_reason"] == "append")
            .expect("append")["records"][0]["sha256"],
        json!(hex_sha256(last.as_bytes()))
    );
}

// ---------------------------------------------------------------------------
// 4. An interior corrupt frame is counted; later lines are not archived.
// ---------------------------------------------------------------------------

#[test]
fn interior_corrupt_frame_is_counted_and_later_lines_are_not_archived() {
    let world = world("dsh-cap-corrupt");
    let dir = session_dir(&world.dsh_root);
    let carrier = dir.join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    let mut middle = frame_of(&[lines[2].clone()]);
    // Damage the frame magic: the frame boundary no longer parses and the
    // reader resynchronizes on the next readable frame. The damaged frame's
    // lines are therefore missing from the evidence while the later frame is
    // still captured.
    middle[0] ^= 0xFF;
    let tail = event("user/message", 3, user(&format!("{SENTINEL} after damage")));
    write_frames(
        &carrier,
        &[frame_of(&lines[..2]), middle, frame_of(&[tail])],
    );

    let config = world.config(false);
    let report = world.capture(&config);
    let dsh = source(&report, "dsh");
    assert_eq!(dsh["files_captured"], 1);
    assert_eq!(dsh["records_added"], 3);
    assert_eq!(dsh["dsh"]["corrupt"], 1);
    let manifests = world.manifest_values();
    assert_eq!(manifests[0]["records"].as_array().expect("records").len(), 3);
    let recorded: Vec<&str> = manifests[0]["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["sha256"].as_str().expect("sha"))
        .collect();
    assert!(
        !recorded.contains(&hex_sha256(lines[2].as_bytes()).as_str()),
        "the damaged frame's line must not be archived"
    );
    let state = world.source_state();
    let entry = state["sources"]
        .as_object()
        .expect("sources")
        .values()
        .find(|value| value["provider"] == json!("dsh"))
        .expect("dsh state");
    assert!(entry["anomalies"]
        .as_array()
        .expect("anomalies")
        .iter()
        .any(|value| value == "corrupt_frame"));
}

// ---------------------------------------------------------------------------
// 5. A rewritten log forces a recapture, links the old generation, keeps old
//    evidence readable.
// ---------------------------------------------------------------------------

#[test]
fn rewritten_log_forces_recapture_and_links_the_previous_capture() {
    let world = world("dsh-cap-rewrite");
    let dir = session_dir(&world.dsh_root);
    let carrier = dir.join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    write_frames(&carrier, &[frame_of(&lines)]);
    let config = world.config(false);
    world.capture(&config);
    let old_manifest = world.manifest_values().remove(0);

    // Rewrite the last line (different content and length).
    let mut rewritten = lines.clone();
    rewritten[2] = event(
        "assistant/message",
        2,
        assistant(&format!("{SENTINEL} rewritten answer with more text")),
    );
    write_frames(&carrier, &[frame_of(&rewritten)]);
    let report = world.capture(&config);
    let dsh = source(&report, "dsh");
    assert_eq!(dsh["files_captured"], 1);
    assert_eq!(dsh["dsh"]["links"][0]["reason"], "last_line_rewritten");
    assert_eq!(
        dsh["dsh"]["links"][0]["rel_path"],
        json!(format!("{PROJECT}/session-{SESSION}/session.v4.jsonl.zstd"))
    );

    let manifests = world.manifest_values();
    assert_eq!(manifests.len(), 2);
    let recaptured = manifests
        .iter()
        .find(|manifest| manifest["source"]["generation"] == 2)
        .expect("new generation");
    assert_eq!(recaptured["source"]["capture_reason"], "forced_recapture");
    assert_eq!(recaptured["source"]["range_start"], 0);
    // The previous generation's manifest and records are untouched.
    assert!(manifests
        .iter()
        .any(|manifest| manifest["capture_id"] == old_manifest["capture_id"]));
    let ledger = world.ledger();
    let entry = ledger["sources"]["dsh"]["files"]
        .as_object()
        .expect("files")
        .values()
        .next()
        .expect("entry");
    assert_eq!(entry["dsh_previous"]["reason"], "last_line_rewritten");

    // A middle-line rewrite (last line unchanged) is classified separately.
    let mut middle = rewritten.clone();
    middle[1] = event("user/message", 1, user(&format!("{SENTINEL} rewritten question")));
    write_frames(&carrier, &[frame_of(&middle)]);
    let report = world.capture(&config);
    let dsh = source(&report, "dsh");
    assert_eq!(dsh["dsh"]["links"][0]["reason"], "prefix_rewritten");
    assert_eq!(world.manifest_values().len(), 3);
}

// ---------------------------------------------------------------------------
// 6. A new format generation restarts the capture and records the link.
// ---------------------------------------------------------------------------

#[test]
fn generation_switch_restarts_from_the_new_carrier_and_links_the_old_one() {
    let world = world("dsh-cap-generation");
    let dir = session_dir(&world.dsh_root);
    let v3 = dir.join("session.v3.jsonl.zstd");
    let v4 = dir.join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    write_frames(&v3, &[frame_of(&lines[..2])]);
    let config = world.config(false);
    world.capture(&config);
    assert_eq!(source(&world.capture(&config), "dsh")["files_skipped_unchanged"], 1);

    write_frames(&v4, &[frame_of(&lines)]);
    let report = world.capture(&config);
    let dsh = source(&report, "dsh");
    assert_eq!(dsh["files_total"], 1, "only the highest generation is captured");
    assert_eq!(dsh["files_captured"], 1);
    // The first two lines are byte-identical to the v3 evidence, so only the
    // third line is new content (records are deduplicated across generations).
    assert_eq!(dsh["records_added"], 1);
    let link = &dsh["dsh"]["links"][0];
    assert_eq!(link["reason"], "generation_switch");
    assert_eq!(
        link["previous_rel_path"],
        json!(format!("{PROJECT}/session-{SESSION}/session.v3.jsonl.zstd"))
    );
    assert_eq!(
        link["rel_path"],
        json!(format!("{PROJECT}/session-{SESSION}/session.v4.jsonl.zstd"))
    );

    let manifests = world.manifest_values();
    assert_eq!(manifests.len(), 2, "old generation evidence is kept");
    let latest = manifests
        .iter()
        .max_by_key(|manifest| manifest["captured_at_ms"].as_i64().unwrap_or(0))
        .expect("latest");
    assert_eq!(latest["source"]["generation"], 2);
    assert_eq!(
        latest["source"]["carrier"],
        json!(format!("{PROJECT}/session-{SESSION}/session.v4.jsonl.zstd"))
    );
    let ledger = world.ledger();
    let entry = ledger["sources"]["dsh"]["files"]
        .as_object()
        .expect("files")
        .values()
        .next()
        .expect("entry");
    assert_eq!(entry["dsh_previous"]["reason"], "generation_switch");
    assert_eq!(entry["dsh_lines"], 3);

    // A later append keeps the recorded migration link while the same carrier
    // stays active (the link is not forgotten on the next run).
    append_frame(
        &v4,
        &[event("user/message", 4, user(&format!("{SENTINEL} appended after migration")))],
    );
    let report = world.capture(&config);
    assert_eq!(source(&report, "dsh")["records_added"], 1);
    let ledger = world.ledger();
    let entry = ledger["sources"]["dsh"]["files"]
        .as_object()
        .expect("files")
        .values()
        .next()
        .expect("entry");
    assert_eq!(entry["dsh_previous"]["reason"], "generation_switch");
    assert_eq!(entry["dsh_lines"], 4);
}

// ---------------------------------------------------------------------------
// 7. `--plan` enumerates only; unknown generations are reported, not guessed.
// ---------------------------------------------------------------------------

#[test]
fn plan_writes_nothing_and_reports_unknown_generations() {
    let world = world("dsh-cap-plan");
    let supported = session_dir(&world.dsh_root).join("session.v4.jsonl.zstd");
    write_frames(&supported, &[frame_of(&base_lines(SENTINEL))]);
    let future_dir = world
        .dsh_root
        .join(PROJECT)
        .join("session-22222222-2222-4222-8222-222222222222");
    write_frames(
        &future_dir.join("session.v5.jsonl.zstd"),
        &[frame_of(&[header(5, "id-future")])],
    );
    let config = world.config(false);
    let before = tree_hash(&world.archive_root);
    let plan = world.plan(&config);
    assert_eq!(before, tree_hash(&world.archive_root), "--plan must write nothing");
    assert_eq!(plan["plan"], true);
    let dsh = source(&plan, "dsh");
    assert_eq!(dsh["files_total"], 1);
    assert_eq!(dsh["files_captured"], 0);
    assert_eq!(dsh["dsh"]["sessions_total"], 1);
    assert_eq!(dsh["dsh"]["sessions_unsupported"], 1);
    assert_eq!(
        dsh["dsh"]["unsupported"],
        json!([format!("{PROJECT}/session-22222222-2222-4222-8222-222222222222")])
    );
    assert!(!world.archive_root.join("state").join("capture-all.json").exists());
}

// ---------------------------------------------------------------------------
// 8. Captured DSH evidence answers from the archive source (readback), and
//    `context: "turn"` is refused explicitly for a DSH record.
// ---------------------------------------------------------------------------

#[test]
fn archived_dsh_session_is_searchable_and_readable_from_the_archive_source() {
    let world = world("dsh-cap-archive");
    let carrier = session_dir(&world.dsh_root).join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    write_frames(&carrier, &[frame_of(&lines)]);
    let config = world.config(true);
    world.capture(&config);

    let response = world.search(
        &config,
        json!({"query": SENTINEL, "sources": ["archive"], "limit": 10}),
    );
    let hit = hit_with_needle(&response, SENTINEL);
    assert_eq!(hit["source_id"], "archive");
    let reference = &hit["ref"];
    let read = world.read(&config, reference);
    assert_eq!(read["ok"], true, "{read}");
    let text = read["text"].as_str().expect("readback text");
    assert!(text.contains(SENTINEL), "readback: {text}");

    let turn = world.read_request(
        &config,
        json!({"ref": reference, "context": "turn"}),
    );
    assert_eq!(turn.code, Some(codes::INVALID_REQUEST));
    assert_eq!(turn.exit_code, 2);

    // An appended line is searchable after the next capture (the archive
    // provider selects only the current generation).
    append_frame(
        &carrier,
        &[event("user/message", 3, user(&format!("{SENTINEL} appended archive")))],
    );
    world.capture(&config);
    let response = world.search(
        &config,
        json!({"query": SENTINEL, "sources": ["archive"], "limit": 10}),
    );
    let results = response["results"].as_array().expect("results");
    assert!(
        results
            .iter()
            .any(|hit| hit["snippet"].as_str().unwrap_or_default().contains("appended archive")),
        "{response}"
    );
}

// ---------------------------------------------------------------------------
// 9. Archive fallback: a DSH session gone from the live root stays in the live
//    index and reads back through the committed logical ranges.
// ---------------------------------------------------------------------------

#[test]
fn deleted_dsh_session_is_indexed_from_archive_and_reads_back() {
    let world = world("dsh-cap-fallback");
    let dir = session_dir(&world.dsh_root);
    let carrier = dir.join("session.v4.jsonl.zstd");
    let lines = base_lines(SENTINEL);
    write_frames(&carrier, &[frame_of(&lines)]);
    let config = world.config(false);
    world.capture(&config);

    let reports = world.index(&config);
    let dsh = world.index_report(&reports, "dsh");
    assert_eq!(dsh["status"], "built", "{dsh}");
    assert_eq!(dsh["detail"]["archive"]["archive_only_sessions"], 0);

    let live = world.search(
        &config,
        json!({"query": SENTINEL, "mode": "exact", "sources": ["dsh"], "limit": 10}),
    );
    let hit = hit_with_needle(&live, SENTINEL);
    assert_eq!(hit["origin"], "live");

    // The live carrier disappears (DSH cleanup / root moved).
    fs::remove_file(&carrier).expect("delete live carrier");
    let reports = world.index(&config);
    let dsh = world.index_report(&reports, "dsh");
    assert_eq!(dsh["detail"]["archive"]["status"], "ok", "{dsh}");
    assert_eq!(dsh["detail"]["archive"]["archive_only_sessions"], 1);

    let archived = world.search(
        &config,
        json!({"query": SENTINEL, "mode": "exact", "sources": ["dsh"], "limit": 10}),
    );
    let hit = hit_with_needle(&archived, SENTINEL);
    assert_eq!(hit["origin"], "archive");
    assert_eq!(hit["ref"]["locator"]["session_id"], SESSION);
    let read = world.read(&config, &hit["ref"]);
    assert_eq!(read["ok"], true, "{read}");
    assert!(read["text"].as_str().unwrap_or_default().contains(SENTINEL));
}
