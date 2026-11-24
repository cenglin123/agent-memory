//! implementation-005 spec 1: one manifest parse per round, with correct
//! invalidation and unchanged fail-closed access control.
//!
//! The evidence store used to be re-parsed for **every** object read
//! (`object_accessible` called `manifests()`), so N evidence reads cost
//! `N x manifests`. These cases pin the three properties that replaced it:
//!
//! 1. N object reads on one archive instance parse the manifest set once, and a
//!    round with nothing new parses nothing at all;
//! 2. a manifest committed by **another** archive instance (another process) is
//!    visible immediately — no stale "orphan" verdict, no stale access map;
//! 3. the access decision is still fail-closed: wrong domain, empty access tags
//!    and an unreferenced object all deny exactly as before.
//!
//! Every fixture is synthetic and lives under a per-test temporary root.

use agent_memory::archive::{manifest_parse_count, Archive, ArchiveError};
use std::fs;
use std::path::{Path, PathBuf};

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

fn claude_root(env: &TestEnv) -> PathBuf {
    let root = env.root("claude-projects");
    for index in 0..3 {
        write_lines(
            &root
                .join("proj")
                .join(format!("11111111-1111-4111-8111-00000000000{index}.jsonl")),
            &[format!(
                "{{\"type\":\"user\",\"sessionId\":\"s{index}\",\"message\":{{\"role\":\"user\",\"content\":\"needle {index}\"}}}}"
            )],
        );
    }
    root
}

fn capture_all(root: &Path, claude_root: &Path, env: &TestEnv) -> Vec<agent_memory::archive::Manifest> {
    let config = env.write_json(
        "snapshot-config.json",
        &env.config_value(
            vec![env.md_source("claude_code", "claude_code", claude_root)],
            vec!["claude_code"],
        ),
    );
    let archive = Archive::open(root).expect("archive opens");
    let (report, code) = agent_memory::capture_all::capture_all(
        &agent_memory::recall::config::load_config(&config).expect("config loads"),
        &agent_memory::capture_all::CaptureAllOptions {
            root: Some(root.to_string_lossy().to_string()),
            domain: DOMAIN.to_string(),
            ..Default::default()
        },
    )
    .expect("capture-all runs");
    assert_eq!(code, 0, "{report:?}");
    let snapshot = archive.manifests().expect("manifests");
    snapshot.iter().cloned().collect()
}

/// The first object of every committed manifest that names one.
fn object_shas(manifests: &[agent_memory::archive::Manifest]) -> Vec<String> {
    manifests
        .iter()
        .flat_map(|manifest| manifest.objects.iter().map(|object| object.sha256.clone()))
        .collect()
}

#[test]
fn repeated_object_reads_parse_the_manifest_set_once() {
    let env = TestEnv::new();
    let root = env.root("snapshot-reads");
    Archive::init(&root).expect("init runtime root");
    let manifests = capture_all(&root, &claude_root(&env), &env);
    assert_eq!(manifests.len(), 3, "one manifest per captured file");
    let objects = object_shas(&manifests);
    assert_eq!(objects.len(), 3, "one whole-file object per file");

    // A fresh instance (the shape every read-only command has): the first read
    // loads and parses the manifest set once.
    let archive = Archive::open(&root).expect("archive opens");
    let before = manifest_parse_count();
    for sha in &objects {
        archive
            .reader(DOMAIN)
            .read_object(sha)
            .expect("object readable");
    }
    let parsed = manifest_parse_count() - before;
    assert_eq!(
        parsed,
        manifests.len() as u64,
        "3 object reads parse the 3 manifests once, not once per read"
    );

    // Steady state: the snapshot is reused, so further reads parse nothing.
    let before = manifest_parse_count();
    for _ in 0..10 {
        for sha in &objects {
            archive
                .reader(DOMAIN)
                .read_object(sha)
                .expect("object readable");
        }
    }
    assert_eq!(
        manifest_parse_count() - before,
        0,
        "30 further object reads parse no manifest body"
    );
    // The device and read costs stay bounded: only the snapshot was listed.
    assert!(archive.manifests().expect("manifests").len() == 3);
}

#[test]
fn a_manifest_committed_elsewhere_is_visible_immediately() {
    let env = TestEnv::new();
    let root = env.root("snapshot-invalidate");
    Archive::init(&root).expect("init runtime root");
    let claude_root = claude_root(&env);
    let first = capture_all(&root, &claude_root, &env);

    // One long-lived reader instance (an MCP service process) and one writer
    // instance (a scheduled capture) share the root.
    let reader = Archive::open(&root).expect("reader opens");
    let reader_manifests = reader.manifests().expect("snapshot");
    assert_eq!(reader_manifests.len(), 3);
    let reconcile_before = reader.reconcile(&[]).expect("reconcile");
    assert!(reconcile_before.orphan_objects.is_empty());

    // The "other process" appends a fourth carrier and captures it.
    write_lines(
        &claude_root
            .join("proj")
            .join("11111111-1111-4111-8111-000000000003.jsonl"),
        &["{\"type\":\"user\",\"sessionId\":\"s3\",\"message\":{\"role\":\"user\",\"content\":\"needle 3\"}}"
            .to_string()],
    );
    let second = capture_all(&root, &claude_root, &env);
    assert_eq!(second.len(), 4, "the second round committed one manifest");

    // The reader sees the new manifest without being reopened, and the new
    // object is readable (no stale "orphan" verdict).
    let refreshed = reader.manifests().expect("refreshed snapshot");
    assert_eq!(refreshed.len(), 4, "appended manifests are picked up");
    assert_eq!(first.len() + 1, refreshed.len());
    let new_objects = object_shas(&second[3..]);
    assert_eq!(new_objects.len(), 1);
    reader
        .reader(DOMAIN)
        .read_object(&new_objects[0])
        .expect("the object committed by the other instance is readable");
    let reconcile_after = reader.reconcile(&[]).expect("reconcile");
    assert!(
        reconcile_after.orphan_objects.is_empty(),
        "a newly committed object is not an orphan: {reconcile_after:?}"
    );
    assert_eq!(reconcile_after.objects_on_disk, reconcile_after.objects_referenced);
}

#[test]
fn access_stays_fail_closed_for_wrong_domain_empty_tags_and_orphans() {
    let env = TestEnv::new();
    let root = env.root("snapshot-access");
    Archive::init(&root).expect("init runtime root");
    let manifests = capture_all(&root, &claude_root(&env), &env);
    let archive = Archive::open(&root).expect("archive opens");
    let sha = object_shas(&manifests).remove(0);

    // Wrong domain: denied, and the denial does not depend on how many objects
    // were read before it (the snapshot is not a "first read wins" cache).
    let denials = |domain: &str| match archive.reader(domain).read_object(&sha) {
        Err(ArchiveError::AccessDenied(message)) => Some(message),
        other => panic!("expected AccessDenied for {domain:?}: {other:?}"),
    };
    let message = denials("other-domain").expect("denied");
    assert!(message.contains("is not visible to domain"), "{message}");
    let _ = denials("other-domain");
    // The allowed domain still reads the very same object.
    archive
        .reader(DOMAIN)
        .read_object(&sha)
        .expect("allowed domain");

    // An unreferenced (orphan) object is denied, never served.
    let orphan = "0".repeat(64);
    match archive.reader(DOMAIN).read_object(&orphan) {
        Err(ArchiveError::AccessDenied(message)) => {
            assert!(message.contains("no committed reference"), "{message}")
        }
        other => panic!("expected AccessDenied for an orphan: {other:?}"),
    }

    // An empty access tag list denies as well, even when another committed
    // reference to the same object allows the domain (strictest intersection):
    // a later capture of the same bytes with no access mapping must not make the
    // object readable.
    let mut unmapped = manifests[0].clone();
    unmapped.capture_id = "cap-unmapped-access".to_string();
    unmapped.objects[0].access.clear();
    let writer = Archive::open(&root).expect("second writer instance");
    writer
        .commit_manifest(&unmapped)
        .expect("commit the unmapped capture");
    match archive.reader(DOMAIN).read_object(&sha) {
        Err(ArchiveError::AccessDenied(message)) => {
            assert!(message.contains("no access mapping"), "{message}")
        }
        other => panic!("expected AccessDenied for an empty access mapping: {other:?}"),
    }
}

#[test]
fn a_removed_manifest_rebuilds_the_snapshot_instead_of_going_stale() {
    let env = TestEnv::new();
    let root = env.root("snapshot-remove");
    Archive::init(&root).expect("init runtime root");
    let manifests = capture_all(&root, &claude_root(&env), &env);
    let archive = Archive::open(&root).expect("archive opens");
    // Warm the snapshot, then drop the manifest that references `sha`.
    assert_eq!(archive.manifests().expect("snapshot").len(), 3);
    let sha = manifests[0].objects[0].sha256.clone();
    archive
        .reader(DOMAIN)
        .read_object(&sha)
        .expect("readable while referenced");
    let names = archive.manifest_names().expect("names");
    fs::remove_file(root.join("archive").join("manifests").join(&names[0]))
        .expect("remove manifest");
    assert_eq!(
        archive.manifests().expect("snapshot").len(),
        2,
        "a removed manifest forces a rebuild instead of serving the old set"
    );
    // The access decision follows the rebuilt snapshot: the object whose only
    // reference is gone is an orphan now and is denied, never served stale.
    match archive.reader(DOMAIN).read_object(&sha) {
        Err(ArchiveError::AccessDenied(message)) => {
            assert!(message.contains("no committed reference"), "{message}")
        }
        other => panic!("expected AccessDenied after the reference disappeared: {other:?}"),
    }
}
