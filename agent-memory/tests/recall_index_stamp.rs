//! Change signals of the JSONL / DSH index watermarks: content probes (head and
//! anchor windows) and the Windows file identity.
//!
//! These live outside `src/recall`'s unit tests because they create files, and
//! `tests/recall_boundary.rs` keeps every write API out of `src/recall`.
//!
//! All fixtures are synthetic and live under a per-case temporary root.

use std::io::Write;

use agent_memory::recall::fulltext::stamp::{file_identity, probes, PROBE_WINDOW};
use agent_memory::recall::fulltext::watermark::JsonlFile;

fn write(path: &std::path::Path, bytes: &[u8]) {
    let mut file = std::fs::File::create(path).expect("create fixture");
    file.write_all(bytes).expect("write fixture");
}

#[test]
fn probes_cover_the_head_and_the_window_ending_at_the_offset() {
    let dir = tempfile::tempdir().expect("temporary root");
    let path = dir.path().join("carrier.jsonl");
    // A non-uniform body: otherwise both windows would hash the same bytes.
    let body: String = (0..20_000).map(|i| if i % 7 == 0 { 'b' } else { 'a' }).collect();
    write(&path, body.as_bytes());

    let (head, anchor) = probes(&path, 20_000).expect("probes");
    assert_eq!(head.len(), 64);
    assert_eq!(anchor.len(), 64);
    let start = 20_000 - PROBE_WINDOW as usize;
    assert_ne!(head, anchor, "the windows do not overlap in this fixture");

    // Appending never moves either probe: they describe `[0, offset)`.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append");
    file.write_all(b"tail\n").expect("append");
    assert_eq!(
        probes(&path, 20_000).expect("probes"),
        (head.clone(), anchor.clone())
    );

    // A same-length rewrite inside the head window changes only the head probe.
    let mut rewritten = body.clone().into_bytes();
    rewritten[0] = b'B';
    write(&path, &rewritten);
    let (head_now, anchor_now) = probes(&path, 20_000).expect("probes");
    assert_ne!(head_now, head);
    assert_eq!(anchor_now, anchor);

    // ... and one inside the anchor window changes only the anchor probe.
    let mut rewritten = body.clone().into_bytes();
    rewritten[start] = b'B';
    write(&path, &rewritten);
    let (head_now, anchor_now) = probes(&path, 20_000).expect("probes");
    assert_eq!(head_now, head);
    assert_ne!(anchor_now, anchor);

    // A missing file has no probes.
    std::fs::remove_file(&path).expect("remove");
    assert!(probes(&path, 20_000).is_none());
    assert!(probes(&path, 0).is_none());
}

#[test]
fn probes_of_an_empty_prefix_are_the_empty_digests() {
    let dir = tempfile::tempdir().expect("temporary root");
    let path = dir.path().join("empty.jsonl");
    write(&path, b"");
    let (head, anchor) = probes(&path, 0).expect("probes");
    assert_eq!(head, anchor);
    assert_eq!(head.len(), 64);
}

#[test]
fn file_identity_is_stable_and_changes_on_replacement() {
    let dir = tempfile::tempdir().expect("temporary root");
    let path = dir.path().join("carrier.jsonl");
    write(&path, b"one\n");
    let first = file_identity(&path);
    assert_eq!(
        file_identity(&path),
        first,
        "the same file keeps its identity"
    );

    // A replacement at the same path (write-to-temp, then rename over) is a
    // different file even with identical bytes.
    let replacement = dir.path().join("replacement.jsonl");
    write(&replacement, b"one\n");
    std::fs::rename(&replacement, &path).expect("replace");
    let second = file_identity(&path);
    if cfg!(windows) {
        assert!(first.is_some(), "Windows answers an identity");
        assert_ne!(first, second, "a replacement is a different file");
    } else {
        assert!(first.is_none(), "off Windows the identity is absent");
        assert!(second.is_none());
    }

    // A missing path has no identity at all.
    std::fs::remove_file(&path).expect("remove");
    assert!(file_identity(&path).is_none());
}

/// A watermark written before implementation-004a carries none of the new fields
/// and must still deserialize (the round then behaves as before and completes the
/// entry).
#[test]
fn a_watermark_without_the_new_fields_deserializes_with_empty_probes() {
    let legacy = r#"{"size":120,"mtime_ns":1700000000000000000,"offset":120,"lines":2}"#;
    let entry: JsonlFile = serde_json::from_str(legacy).expect("legacy entry");
    assert_eq!(entry.size, 120);
    assert_eq!(entry.lines, 2);
    assert!(entry.head_probe.is_empty());
    assert!(entry.anchor_probe.is_empty());
    assert!(entry.file_identity.is_empty());

    // A round writes the canonical shape back.
    let encoded = serde_json::to_string(&entry).expect("encode");
    assert!(encoded.contains("\"head_probe\""), "{encoded}");
}
