//! §D follow-up (verify-001 P2): one `index` run parses the manifest set at
//! most **once per source build**. Before the fix every pending carrier
//! rebuild loaded and parsed the whole manifest set again, so a first round
//! over K carriers read `K x manifests` bytes.
//!
//! The assertion uses the process-wide manifest parse counter
//! (`agent_memory::archive::manifest_parse_count`), which is platform
//! independent and counts parsed manifest files, not bytes: the process reads
//! exactly one manifest set, never `carriers x manifests`. This file is its own
//! test binary so no other test can move the counter while it runs.
//!
//! Every fixture is synthetic and lives under a per-test temporary root.

use agent_memory::archive::{manifest_parse_count, Archive};
use agent_memory::recall::config::load_config;
use agent_memory::recall::fulltext::build::index_all;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

mod support;

use support::TestEnv;

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent dir");
    }
    fs::write(path, body).expect("write fixture file");
}

/// A Claude transcript that projects no rows (bookkeeping records only).
fn zero_carrier(session: &str) -> Vec<String> {
    vec![
        json!({"type": "mode", "mode": "plan", "sessionId": session}).to_string(),
        json!({"type": "system", "subtype": "init", "sessionId": session}).to_string(),
    ]
}

/// World with `carriers` zero-row Claude transcripts, captured and then removed
/// from the live root so every one of them is archive-only at index time.
fn world(name: &str, carriers: usize) -> (TestEnv, PathBuf, PathBuf) {
    let env = TestEnv::new();
    let archive_root = env.root(&format!("{name}-archive"));
    Archive::init(&archive_root).expect("init runtime root");
    let cache_root = env.root(&format!("{name}-cache"));
    let claude_root = env.root(&format!("{name}-claude"));
    let project = claude_root.join("proj");
    for index in 0..carriers {
        let session = format!("00000000-0000-4000-8000-{index:012}");
        write_lines(
            &project.join(format!("carrier-{index:03}.jsonl")),
            &zero_carrier(&session),
        );
    }
    let config = env.write_json(
        &format!("{name}-recall.json"),
        &json!({
            "version": 2,
            "sources": [
                {"source_id": "claude_code", "kind": "claude_code",
                 "root": claude_root.to_string_lossy(), "enabled": true},
                {"source_id": "archive", "kind": "archive",
                 "root": archive_root.to_string_lossy(), "enabled": false}
            ],
            "source_order": ["claude_code"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false},
            "cache_root": cache_root.to_string_lossy()
        }),
    );
    {
        let parsed = load_config(&config).expect("config loads");
        let options = agent_memory::capture_all::CaptureAllOptions {
            root: None,
            domain: "default".to_string(),
            only_source: None,
            session_since: None,
            plan: false,
            skip_if_locked: false,
        };
        let (report, code) =
            agent_memory::capture_all::capture_all(&parsed, &options).expect("capture-all runs");
        assert_eq!(code, 0, "capture-all failed: {report:?}");
        assert!(report.ok, "capture-all not ok");
    }
    for index in 0..carriers {
        fs::remove_file(project.join(format!("carrier-{index:03}.jsonl")))
            .expect("delete the live carrier");
    }
    (env, archive_root, config)
}

fn index_source(config: &Path) -> u64 {
    let parsed = load_config(config).expect("config loads");
    let reports = index_all(&parsed, Some("claude_code"), false).expect("index runs");
    let report = reports
        .iter()
        .find(|report| report.source_id == "claude_code")
        .expect("claude report");
    let archive = &report.detail.as_ref().expect("detail")["archive"];
    assert_eq!(archive["status"], "ok", "{archive}");
    let added = archive["added"].as_u64().unwrap_or(0);
    added
}

#[test]
fn one_manifest_set_is_parsed_per_source_build_not_per_carrier() {
    for carriers in [3usize, 12] {
        let (_env, archive_root, config) = world(&format!("snapshot-{carriers}"), carriers);
        let manifest_files = Archive::open(&archive_root)
            .expect("archive opens")
            .manifest_names()
            .expect("manifest names")
            .len() as u64;
        assert!(
            manifest_files >= carriers as u64,
            "each captured carrier has a manifest: {manifest_files} for {carriers} carriers"
        );

        let before = manifest_parse_count();
        let added = index_source(&config);
        let parsed = manifest_parse_count() - before;

        assert_eq!(added, carriers as u64, "every carrier is projected once");
        assert_eq!(
            parsed, manifest_files,
            "K={carriers}: the manifest set is parsed once ({manifest_files} files), \
             not once per carrier ({} reads)",
            carriers as u64 * manifest_files
        );
        assert!(
            parsed < carriers as u64 * manifest_files,
            "K={carriers}: reads must not scale with the carrier count"
        );

        // Second run: nothing is pending, so no manifest body is parsed at all.
        let before = manifest_parse_count();
        let added = index_source(&config);
        assert_eq!(added, 0, "K={carriers}: the second run registers nothing");
        assert_eq!(
            manifest_parse_count() - before,
            0,
            "K={carriers}: a round with nothing pending parses no manifest"
        );
    }
}
