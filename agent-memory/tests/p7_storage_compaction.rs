//! Stage-2 S2.5 acceptance: object storage compression (explicit zstd
//! encoding), the `compact` migration command, and the audit scan over
//! compressed objects.
//!
//! Every root is a temporary directory and every corpus is synthetic. No real
//! session, knowledge base or production Archive is read.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_memory::archive::{hex_sha256, Archive};
use agent_memory::attachment::{capture_attachment, AttachmentRequest};
use agent_memory::capture::capture_source;
use agent_memory::model::ContentRef;
use agent_memory::recall::config::load_config;
use agent_memory::recall::read::{memory_read, ReadRequest};
use agent_memory::recall::search::{memory_search, SearchRequest};
use agent_memory::replay::{read_derived, replay, ReplayIdentity};
use serde_json::{json, Value};

const DOMAIN: &str = "default";
const NEEDLE: &str = "s25compactedneedle";

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn init_root() -> (tempfile::TempDir, Archive) {
    // A plain `tempfile::tempdir()` may land inside another case's redirected
    // TEMP root (`support::TestEnv` in this same binary) and be deleted when
    // that case drops: `stable_tempdir` always uses the original system temp.
    let temp = support::stable_tempdir();
    let archive = Archive::init(temp.path()).unwrap();
    (temp, archive)
}

fn write_jsonl(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
    let path = dir.join(name);
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    path
}

fn codex_lines(session: &str, extra: &[String]) -> Vec<String> {
    let mut lines = vec![
        support::codex_meta(session),
        support::codex_user(NEEDLE),
        support::codex_assistant("s25 answer"),
    ];
    lines.extend(extra.iter().cloned());
    lines
}

/// Re-encode a captured archive to the *legacy* on-disk shape: decode every
/// zstd object into its plain file and drop the `.zst` file. This models the
/// S3 production state that `compact` must migrate (objects captured before
/// this change), without needing an old binary.
fn downgrade_to_legacy_plain(root: &Path) {
    let objects = root.join("archive").join("objects");
    for entry in walk_files(&objects) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(sha) = name.strip_suffix(".zst") else {
            continue;
        };
        let compressed = std::fs::read(&entry.path()).unwrap();
        let decoded = zstd::stream::decode_all(compressed.as_slice()).unwrap();
        let plain = entry.path().parent().unwrap().join(sha);
        std::fs::write(&plain, &decoded).unwrap();
        std::fs::remove_file(entry.path()).unwrap();
    }
}

fn walk_files(dir: &Path) -> Vec<std::fs::DirEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_files(&path));
        } else {
            out.push(entry);
        }
    }
    out
}

/// Every file below `root` as (sorted relative path, sha256) digest — proves a
/// run wrote nothing.
fn tree_digest(root: &Path) -> String {
    let mut entries: Vec<String> = walk_files(root)
        .iter()
        .map(|entry| {
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            format!("{relative}|{}", hex_sha256(&std::fs::read(entry.path()).unwrap()))
        })
        .collect();
    entries.sort();
    hex_sha256(entries.join("\n").as_bytes())
}

fn object_file(root: &Path, sha: &str) -> Option<PathBuf> {
    let dir = root
        .join("archive")
        .join("objects")
        .join(&sha[0..2])
        .join(&sha[2..4]);
    let zstd = dir.join(format!("{sha}.zst"));
    if zstd.exists() {
        return Some(zstd);
    }
    let plain = dir.join(sha);
    plain.exists().then_some(plain)
}

fn manifest_objects(root: &Path) -> Vec<(String, String, u64)> {
    let manifests_dir = root.join("archive").join("manifests");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&manifests_dir).unwrap().flatten() {
        let manifest: Value = serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
        for object in manifest["objects"].as_array().unwrap() {
            out.push((
                object["sha256"].as_str().unwrap().to_string(),
                object["role"].as_str().unwrap().to_string(),
                object["bytes"].as_u64().unwrap(),
            ));
        }
    }
    out
}

fn v2_archive_config(root: &Path, config_path: &Path) -> PathBuf {
    let value = json!({
        "version": 2,
        "sources": [{
            "source_id": "ar", "kind": "archive",
            "root": root.to_string_lossy(), "enabled": true
        }],
        "source_order": ["ar"],
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": DOMAIN,
        "telemetry": {"enabled": false}
    });
    std::fs::write(config_path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    config_path.to_path_buf()
}

fn search(config_path: &Path, query: &str) -> agent_memory::recall::search::SearchResponse {
    let config = load_config(config_path).unwrap();
    let request = SearchRequest::parse(&json!({"query": query})).unwrap();
    memory_search(&config, &request)
}

// ---------------------------------------------------------------------------
// 1. Fresh JSONL capture stores its object compressed; readback is byte-exact
// ---------------------------------------------------------------------------

#[test]
fn jsonl_capture_stores_zstd_and_reads_back_byte_exact() {
    let (temp, archive) = init_root();
    let src = write_jsonl(temp.path(), "rollout-s25.jsonl", &codex_lines("s25-a", &[]));
    let report = capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert_eq!(report.status, "captured");

    let whole_sha = report.sha256.clone().unwrap();
    let path = object_file(archive.root(), &whole_sha).expect("object on disk");
    assert!(
        path.to_string_lossy().ends_with(".zst"),
        "JSONL whole-file object must take the zstd encoding: {path:?}"
    );
    // Readback: whole object and each record are byte-exact.
    let source_bytes = std::fs::read(&src).unwrap();
    assert_eq!(archive.read_object_bytes(&whole_sha).unwrap(), source_bytes);
    let records = archive.load_records().unwrap();
    let first = records.records.values().next().unwrap();
    let record_bytes = archive
        .reader(DOMAIN)
        .read_record(&hex_sha256(
            &archive.read_object_bytes(&first.object).unwrap()
                [first.offset as usize..(first.offset + first.len) as usize],
        ))
        .unwrap();
    assert!(!record_bytes.is_empty());

    // CLI read path decodes transparently and verifies.
    let out = temp.path().join("readback.bin");
    let probe = cli()
        .args(["read", "--root"])
        .arg(archive.root())
        .args(["--sha256", &whole_sha, "--domain", DOMAIN, "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(probe.status.success(), "{:?}", probe.stderr);
    assert_eq!(std::fs::read(&out).unwrap(), source_bytes);
}

// ---------------------------------------------------------------------------
// 2. A `.jsonl.zst` whole-file capture is never recompressed; its decoded
//    sibling object is stored compressed.
// ---------------------------------------------------------------------------

#[test]
fn compressed_rollout_whole_file_stays_plain() {
    let (temp, archive) = init_root();
    let plain = write_jsonl(temp.path(), "rollout-zst-src.jsonl", &codex_lines("s25-z", &[]));
    let zst = support::build_codex_zst(&plain);
    let zst_bytes = std::fs::read(&zst).unwrap();

    let report = capture_source(&archive, &zst, "codex", DOMAIN, None).unwrap();
    assert_eq!(report.status, "captured", "{report:?}");

    let objects = manifest_objects(archive.root());
    assert_eq!(objects.len(), 2, "whole_file + decoded roles");
    let whole = objects.iter().find(|(_, role, _)| role == "whole_file").unwrap();
    let decoded = objects.iter().find(|(_, role, _)| role == "decoded").unwrap();

    // The raw `.zst` file object keeps the plain encoding (no double zstd).
    let whole_path = object_file(archive.root(), &whole.0).unwrap();
    assert!(
        !whole_path.to_string_lossy().ends_with(".zst"),
        "already-compressed whole-file object must stay plain: {whole_path:?}"
    );
    assert_eq!(std::fs::read(&whole_path).unwrap(), zst_bytes);
    assert_eq!(archive.read_object_bytes(&whole.0).unwrap(), zst_bytes);

    // The decoded rollout content is compressible and takes the zstd encoding.
    let decoded_path = object_file(archive.root(), &decoded.0).unwrap();
    assert!(decoded_path.to_string_lossy().ends_with(".zst"));
    let original_text = std::fs::read(&plain).unwrap();
    assert_eq!(archive.read_object_bytes(&decoded.0).unwrap(), original_text);
}

// ---------------------------------------------------------------------------
// 3. compact migrates legacy plain objects; every reader result is identical.
// ---------------------------------------------------------------------------

#[test]
fn compact_migrates_and_all_readers_agree() {
    let env = support::TestEnv::new();
    let (temp, archive) = init_root();
    let src = write_jsonl(temp.path(), "rollout-s25-m.jsonl", &codex_lines("s25-m", &[]));
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    downgrade_to_legacy_plain(archive.root());

    // --- snapshot every reader BEFORE compact -------------------------------
    let readbacks_before: Vec<(String, Vec<u8>)> = manifest_objects(archive.root())
        .into_iter()
        .map(|(sha, _, _)| {
            let bytes = archive.read_object_bytes(&sha).unwrap();
            (sha, bytes)
        })
        .collect();
    let identity = ReplayIdentity::current();
    let replay_before = replay(&archive, &identity, DOMAIN).unwrap();
    assert!(replay_before.ok, "{replay_before:?}");
    let retrieval_sha_before = replay_before.retrieval.as_ref().map(|r| r.sha256.clone());
    let units_before = replay_before.retrieval.as_ref().map(|r| r.units);
    let snapshot_before = agent_memory::replay::checkpoint_snapshot(&archive).unwrap();
    let derived_refs_before: Vec<String> = snapshot_before
        .entries
        .values()
        .map(|entry| {
            format!(
                "derived:{}:{}",
                snapshot_before.generation.expect("generation present"),
                entry.key
            )
        })
        .collect();
    assert!(!derived_refs_before.is_empty());

    let config_path = v2_archive_config(archive.root(), &env.root("cfg").join("v2.json"));
    let search_before = search(&config_path, NEEDLE);
    assert_eq!(search_before.exit_code, 0, "{}", serde_json::to_string(&search_before).unwrap());
    assert_eq!(search_before.results.len(), 1);
    let read_before = {
        let config = load_config(&config_path).unwrap();
        let request = ReadRequest::parse(&json!({"ref": search_before.results[0].reference})).unwrap();
        memory_read(&config, &request)
    };
    assert!(read_before.ok);

    // Every object is plain before the migration.
    for (sha, _, _) in manifest_objects(archive.root()) {
        assert!(!object_file(archive.root(), &sha)
            .unwrap()
            .to_string_lossy()
            .ends_with(".zst"));
    }

    // --- compact -------------------------------------------------------------
    let report = agent_memory::compact::compact(&archive, false).unwrap();
    assert!(report.ok, "{report:?}");
    assert!(report.compressed >= 1, "plain objects must migrate: {report:?}");
    assert!(report.saved_bytes > 0);
    assert_eq!(report.corrupted, 0);

    // --- all reader results are byte-identical AFTER compact -----------------
    for (sha, bytes) in &readbacks_before {
        assert_eq!(&archive.read_object_bytes(sha).unwrap(), bytes);
    }
    let replay_after = replay(&archive, &ReplayIdentity::current(), DOMAIN).unwrap();
    assert!(replay_after.ok, "{replay_after:?}");
    assert_eq!(
        replay_after.retrieval.as_ref().map(|r| r.sha256.clone()),
        retrieval_sha_before,
        "retrieval artifact must be byte-identical across compact"
    );
    assert_eq!(replay_after.retrieval.as_ref().map(|r| r.units), units_before);
    // Derived artifact readback is byte-identical.
    for derived_ref in &derived_refs_before {
        let (bytes, meta) = read_derived(&archive, derived_ref, DOMAIN).unwrap();
        assert!(meta.verified);
        assert!(!bytes.is_empty());
    }
    // Recall archive search + readback identical.
    let search_after = search(&config_path, NEEDLE);
    assert_eq!(search_after.exit_code, 0);
    assert_eq!(search_after.results.len(), search_before.results.len());
    assert_eq!(
        search_after.results[0].reference.locator.record_sha256,
        search_before.results[0].reference.locator.record_sha256
    );
    let config = load_config(&config_path).unwrap();
    let read_after = {
        let request = ReadRequest::parse(&json!({"ref": search_after.results[0].reference})).unwrap();
        memory_read(&config, &request)
    };
    assert_eq!(read_after.text, read_before.text);

    // Every migrated object now lives under its `.zst` name.
    for (sha, _, _) in manifest_objects(archive.root()) {
        assert!(object_file(archive.root(), &sha)
            .unwrap()
            .to_string_lossy()
            .ends_with(".zst"));
    }
}

// ---------------------------------------------------------------------------
// 4. compact is idempotent.
// ---------------------------------------------------------------------------

#[test]
fn compact_is_idempotent() {
    let (temp, archive) = init_root();
    let src = write_jsonl(temp.path(), "rollout-s25-i.jsonl", &codex_lines("s25-i", &[]));
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    downgrade_to_legacy_plain(archive.root());

    let first = agent_memory::compact::compact(&archive, false).unwrap();
    assert!(first.compressed >= 1);
    let second = agent_memory::compact::compact(&archive, false).unwrap();
    assert_eq!(second.compressed, 0);
    assert_eq!(second.converged, 0);
    assert_eq!(second.saved_bytes, 0);
    assert_eq!(second.objects_scanned, first.objects_scanned);
    assert_eq!(second.already_compressed, first.objects_scanned);
    assert!(second.ok);
    // And the digest of the whole store is unchanged by the no-op run.
    let digest_before = tree_digest(archive.root());
    agent_memory::compact::compact(&archive, false).unwrap();
    assert_eq!(tree_digest(archive.root()), digest_before);
}

// ---------------------------------------------------------------------------
// 5. Dual-encoding crash window: reads stay correct and compact converges.
// ---------------------------------------------------------------------------

#[test]
fn dual_encoding_crash_window_reads_and_converges() {
    let (temp, archive) = init_root();
    let src = write_jsonl(temp.path(), "rollout-s25-d.jsonl", &codex_lines("s25-d", &[]));
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    let objects = manifest_objects(archive.root());
    let (sha, _, bytes) = &objects[0];

    // Simulate the crash window: the compressed object was renamed into place
    // but the plain original was not removed yet.
    let decoded = archive.read_object_bytes(sha).unwrap();
    let dir = archive
        .root()
        .join("archive")
        .join("objects")
        .join(&sha[0..2])
        .join(&sha[2..4]);
    std::fs::write(dir.join(sha), &decoded).unwrap();
    assert_eq!(*bytes, decoded.len() as u64);

    // Reads prefer the verified zstd file and stay byte-exact.
    assert_eq!(archive.object_encoding(sha), Some(agent_memory::archive::ObjectEncoding::Zstd));
    assert_eq!(archive.read_object_bytes(sha).unwrap(), decoded);
    let reconcile = archive.reconcile(&[]).unwrap();
    assert_eq!(reconcile.dual_encoding, vec![sha.clone()]);
    assert_eq!(reconcile.objects_on_disk, objects.len() as u64);

    // The next compact verifies the zstd side and removes the plain leftover.
    let report = agent_memory::compact::compact(&archive, false).unwrap();
    assert_eq!(report.converged, 1, "{report:?}");
    assert!(report.ok);
    assert!(!dir.join(sha).exists());
    assert_eq!(archive.reconcile(&[]).unwrap().dual_encoding, Vec::<String>::new());
    assert_eq!(archive.read_object_bytes(sha).unwrap(), decoded);
}

// ---------------------------------------------------------------------------
// 6. --dry-run estimates without writing anything.
// ---------------------------------------------------------------------------

#[test]
fn dry_run_writes_nothing() {
    let (temp, archive) = init_root();
    let src = write_jsonl(temp.path(), "rollout-s25-dr.jsonl", &codex_lines("s25-dr", &[]));
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    downgrade_to_legacy_plain(archive.root());

    let digest_before = tree_digest(archive.root());
    let report = agent_memory::compact::compact(&archive, true).unwrap();
    assert!(report.dry_run);
    assert!(report.compressed >= 1, "{report:?}");
    assert!(report.saved_bytes > 0);
    // Runtime root is byte-identical (no staging file, no rename, no removal).
    assert_eq!(tree_digest(archive.root()), digest_before);
    // The projection matches a real run of the same store.
    let real = agent_memory::compact::compact(&archive, false).unwrap();
    assert_eq!(real.compressed, report.compressed);
    assert_eq!(real.saved_bytes, report.saved_bytes);
}

// ---------------------------------------------------------------------------
// 7. Same content captured twice deduplicates to one object.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_content_second_capture_dedupes() {
    let (temp, archive) = init_root();
    let lines = codex_lines("s25-dupe", &[]);
    let a = write_jsonl(temp.path(), "rollout-dupe-a.jsonl", &lines);
    let b = write_jsonl(temp.path(), "rollout-dupe-b.jsonl", &lines);
    let first = capture_source(&archive, &a, "codex", DOMAIN, None).unwrap();
    let second = capture_source(&archive, &b, "codex", DOMAIN, None).unwrap();
    assert_eq!(first.status, "captured");
    assert_eq!(second.status, "captured");
    assert_eq!(first.sha256, second.sha256, "same content, same address");
    let reconcile = archive.reconcile(&[]).unwrap();
    assert_eq!(reconcile.objects_on_disk, 1, "dedupe: one object for both captures");
    // The dedupe hit did not create a second encoding of the same sha.
    assert!(reconcile.dual_encoding.is_empty());
}

// ---------------------------------------------------------------------------
// 8. SQLite evidence objects stay plain (their payload is already zstd).
// ---------------------------------------------------------------------------

#[test]
fn sqlite_objects_are_never_double_compressed() {
    let (temp, archive) = init_root();
    let db = temp.path().join("opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, parent_id TEXT,
            time_created INTEGER, time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT);
        INSERT INTO session VALUES ('ses_1', 'synthetic', NULL, 1, 2);
        INSERT INTO message VALUES ('msg_1', 'ses_1', 10, 10, '{"role":"user"}');
        "#,
    )
    .unwrap();
    for index in 1..=40 {
        conn.execute(
            "INSERT INTO part VALUES (?1, 'msg_1', 'ses_1', ?2, ?2, ?3)",
            rusqlite::params![
                format!("prt_{index:02}"),
                100 + index,
                json!({"type":"text","text":format!("part body {index} padding padding")}).to_string()
            ],
        )
        .unwrap();
    }

    let report = capture_source(&archive, &db, "opencode", DOMAIN, None).unwrap();
    assert_eq!(report.status, "captured", "{report:?}");
    // Every object of the SQLite evidence unit is a zstd payload already —
    // none of them may be wrapped into a second zstd layer.
    for (sha, _, _) in manifest_objects(archive.root()) {
        let path = object_file(archive.root(), &sha).unwrap();
        assert!(
            !path.to_string_lossy().ends_with(".zst"),
            "SQLite zstd payload stored double-compressed: {path:?}"
        );
        assert_eq!(
            archive.read_object_bytes(&sha).unwrap().len() as u64,
            manifest_objects(archive.root())
                .iter()
                .find(|(s, _, _)| s == &sha)
                .unwrap()
                .2
        );
    }
    // A delta capture stays plain as well.
    conn.execute(
        "INSERT INTO message VALUES ('msg_2', 'ses_1', 20, 20, '{\"role\":\"assistant\"}')",
        [],
    )
    .unwrap();
    let delta = capture_source(&archive, &db, "opencode", DOMAIN, None).unwrap();
    assert_eq!(delta.status, "captured", "{delta:?}");
    for (sha, _, _) in manifest_objects(archive.root()) {
        let path = object_file(archive.root(), &sha).unwrap();
        assert!(!path.to_string_lossy().ends_with(".zst"), "{path:?}");
    }
}

// ---------------------------------------------------------------------------
// 9. Attachments: compressible content takes zstd, incompressible stays plain.
// ---------------------------------------------------------------------------

#[test]
fn attachments_compress_by_worth_not_by_media_type() {
    let (temp, archive) = init_root();
    let session = temp.path().join("session-s25.jsonl");
    std::fs::write(&session, b"{}\n").unwrap();

    let attach = |name: &str| {
        let dir = temp.path().join("attachments");
        std::fs::create_dir_all(&dir).unwrap();
        AttachmentRequest {
            provider: "codex".into(),
            source: session.to_string_lossy().into(),
            content_ref: ContentRef::Sidecar { rel_path: name.into() },
            target: "local_file".into(),
            roots: vec![dir.to_string_lossy().into()],
            access: vec![DOMAIN.into()],
            max_bytes: None,
            record_sha256: None,
            media_type: None,
        }
    };

    // Repetitive text compresses well below the threshold.
    let dir = temp.path().join("attachments");
    std::fs::create_dir_all(&dir).unwrap();
    let text_attachment = dir.join("notes.txt");
    std::fs::write(&text_attachment, "attachment line\n".repeat(2000)).unwrap();
    let text_report = capture_attachment(&archive, &attach("notes.txt")).unwrap();
    assert!(text_report.ok, "{text_report:?}");
    let text_sha = text_report.sha256.clone().unwrap();
    assert!(object_file(archive.root(), &text_sha)
        .unwrap()
        .to_string_lossy()
        .ends_with(".zst"));
    let expected = std::fs::read(&text_attachment).unwrap();
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&text_report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        expected
    );

    // High-entropy content is stored verbatim (worth test fails).
    let mut value: u64 = 0x243F_6A88_85A3_08D3;
    let mut noise = Vec::with_capacity(48 * 1024);
    for _ in 0..48 * 1024 / 8 {
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        noise.extend_from_slice(&value.to_le_bytes());
    }
    let bin_attachment = dir.join("blob.bin");
    std::fs::write(&bin_attachment, &noise).unwrap();
    let bin_report = capture_attachment(&archive, &attach("blob.bin")).unwrap();
    assert!(bin_report.ok, "{bin_report:?}");
    let bin_sha = bin_report.sha256.clone().unwrap();
    assert!(
        !object_file(archive.root(), &bin_sha)
            .unwrap()
            .to_string_lossy()
            .ends_with(".zst"),
        "incompressible attachment must stay plain"
    );
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&bin_report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        noise
    );
}

// ---------------------------------------------------------------------------
// 10. status/reconcile report per-encoding storage statistics.
// ---------------------------------------------------------------------------

#[test]
fn status_reports_storage_statistics() {
    let (temp, archive) = init_root();
    let src = write_jsonl(temp.path(), "rollout-s25-st.jsonl", &codex_lines("s25-st", &[]));
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();

    let probe = cli()
        .args(["status", "--root"])
        .arg(archive.root())
        .output()
        .unwrap();
    assert!(probe.status.success());
    let status: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let storage = &status["reconcile"]["storage"];
    assert_eq!(storage["zstdObjects"].as_u64(), Some(1));
    assert_eq!(storage["plainObjects"].as_u64(), Some(0));
    let original = storage["zstdOriginalBytes"].as_u64().unwrap();
    let stored = storage["zstdStoredBytes"].as_u64().unwrap();
    assert!(stored > 0 && stored < original, "stored {stored} < original {original}");
    assert_eq!(
        status["reconcile"]["objects_on_disk"].as_u64(),
        Some(1)
    );
    assert_eq!(
        status["reconcile"]["dual_encoding"].as_array().map(|a| a.len()),
        Some(0)
    );
    // compact --dry-run via CLI keeps the same contract (exit 0, no writes).
    let digest_before = tree_digest(archive.root());
    let probe = cli()
        .args(["compact", "--root"])
        .arg(archive.root())
        .args(["--dry-run"])
        .output()
        .unwrap();
    assert!(probe.status.success(), "{:?}", probe.stderr);
    let report: Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(report["dryRun"], json!(true));
    assert_eq!(report["alreadyCompressed"].as_u64(), Some(1));
    assert_eq!(tree_digest(archive.root()), digest_before);
}

// ---------------------------------------------------------------------------
// 11. audit-scan counts compressed-object hits and never leaks values.
// ---------------------------------------------------------------------------

#[test]
fn audit_scan_counts_hits_without_leaking_values() {
    let (temp, archive) = init_root();
    // A synthetic session whose text contains a fake key (assembled at runtime
    // so the pattern literal never appears in this deliverable).
    let fake_key = format!("sk-{}{}", "sample", "abcdefghijklmnop");
    let lines = vec![
        support::codex_meta("s25-scan"),
        support::codex_user(&format!("leak check bearer {fake_key} end")),
    ];
    let src = write_jsonl(temp.path(), "rollout-s25-scan.jsonl", &lines);
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    assert!(
        archive.read_object_bytes(&manifest_objects(archive.root())[0].0)
            .is_ok()
    );

    let patterns = json!([
        {"name": "assigned-secret", "pattern": "(?i)\\b(api[_-]?key|secret|password|credential|access[_-]?token|bearer)\\b[\"']?\\s*[:=]\\s*[\"']?([A-Za-z0-9_\\-\\.\\/\\+]{12,})"},
        {"name": "known-key-prefix", "pattern": "(?:sk-[A-Za-z0-9]{16,}|ghp_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16})"}
    ]);
    let probe = cli()
        .args(["audit-scan", "--root"])
        .arg(archive.root())
        .arg("--patterns")
        .arg(serde_json::to_string(&patterns).unwrap())
        .output()
        .unwrap();
    assert!(probe.status.success(), "{:?}", probe.stderr);
    let stdout = String::from_utf8(probe.stdout.clone()).unwrap();
    let report: Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["filesWithHits"].as_u64(), Some(1));
    assert_eq!(report["hitsByRule"]["known-key-prefix"].as_u64(), Some(1));
    assert_eq!(report["hitsByRule"].get("assigned-secret"), None);
    // Counts only: the matched value never appears in the report.
    assert!(!stdout.contains(&fake_key), "audit report leaked the matched value");
    assert!(!stdout.contains(NEEDLE), "audit report leaked object content");

    // The clean part of the same corpus produces no hit.
    let clean = write_jsonl(temp.path(), "rollout-s25-clean.jsonl", &codex_lines("s25-clean", &[]));
    capture_source(&archive, &clean, "codex", DOMAIN, None).unwrap();
    let probe = cli()
        .args(["audit-scan", "--root"])
        .arg(archive.root())
        .arg("--patterns")
        .arg(serde_json::to_string(&patterns).unwrap())
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(report["filesWithHits"].as_u64(), Some(1), "only the planted object hits");
    assert!(
        report["objects"].as_u64().unwrap() >= 2,
        "both captured objects were scanned"
    );
}

// ---------------------------------------------------------------------------
// 12. compact CLI: exit codes and lock semantics.
// ---------------------------------------------------------------------------

#[test]
fn compact_cli_exit_codes() {
    let (temp, archive) = init_root();
    let src = write_jsonl(temp.path(), "rollout-s25-cli.jsonl", &codex_lines("s25-cli", &[]));
    capture_source(&archive, &src, "codex", DOMAIN, None).unwrap();
    downgrade_to_legacy_plain(archive.root());

    // A held capture lock fails compact with the stable lock exit code.
    let _lock = archive.acquire_lock().unwrap();
    let probe = cli()
        .args(["compact", "--root"])
        .arg(archive.root())
        .output()
        .unwrap();
    assert_eq!(probe.status.code(), Some(4), "{:?}", probe.stderr);
    assert!(String::from_utf8(probe.stderr).unwrap().contains("\"ok\":false"));
    drop(_lock);

    let probe = cli()
        .args(["compact", "--root"])
        .arg(archive.root())
        .output()
        .unwrap();
    assert!(probe.status.success(), "{:?}", probe.stderr);
    let report: Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(report["ok"], json!(true));
    assert!(report["compressed"].as_u64().unwrap() >= 1);

    // Uninitialized root -> io/state exit code 3.
    let probe = cli()
        .args(["compact", "--root"])
        .arg(temp.path().join("no-such-root"))
        .output()
        .unwrap();
    assert_eq!(probe.status.code(), Some(3));

    // Missing --root -> usage exit 2.
    let probe = cli().args(["compact"]).output().unwrap();
    assert_eq!(probe.status.code(), Some(2));
}

// ---------------------------------------------------------------------------
// 13. An undecodable unreferenced `.zst` object (external damage) is counted
//     as unreadable and stays an orphan; it never fails status or capture.
// ---------------------------------------------------------------------------

#[test]
fn undecodable_orphan_zst_never_fails_status_or_capture() {
    let (temp, archive) = init_root();
    let first = write_jsonl(temp.path(), "rollout-s25-orphan-a.jsonl", &codex_lines("s25-orphan-a", &[]));
    capture_source(&archive, &first, "codex", DOMAIN, None).unwrap();

    let sha = hex_sha256(b"hand-placed orphan");
    let dir = archive
        .root()
        .join("archive")
        .join("objects")
        .join(&sha[0..2])
        .join(&sha[2..4]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{sha}.zst")), b"\x00not a zstd frame").unwrap();

    let probe = cli().args(["status", "--root"]).arg(archive.root()).output().unwrap();
    assert!(probe.status.success(), "{}", String::from_utf8_lossy(&probe.stderr));
    let status: Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(status["reconcile"]["storage"]["unreadableObjects"].as_u64(), Some(1));
    let orphans = status["reconcile"]["orphan_objects"].as_array().unwrap();
    assert!(
        orphans.iter().any(|o| o.as_str().is_some_and(|s| s.contains(&sha))),
        "the damaged object stays reported as an orphan: {orphans:?}"
    );

    let second = write_jsonl(temp.path(), "rollout-s25-orphan-b.jsonl", &codex_lines("s25-orphan-b", &[]));
    let report = capture_source(&archive, &second, "codex", DOMAIN, None).unwrap();
    assert!(report.ok, "capture must not fail because of an unrelated damaged orphan");
}
