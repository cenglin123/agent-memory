//! P4 acceptance tests: attachments and complete ContentRef archiving.
//!
//! Every case uses a `tempfile` root and synthetic files only. No real session
//! directory, database or attachment is read.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_memory::archive::{
    attachment_id, hex_sha256, Archive, ArchiveError, ATTACHMENT_STATUS_ARCHIVED,
    ATTACHMENT_STATUS_EXTERNAL_REFERENCE, ATTACHMENT_STATUS_MISSING,
    ATTACHMENT_STATUS_OUT_OF_BOUNDS, ATTACHMENT_STATUS_OVER_LIMIT,
    ATTACHMENT_STATUS_PATH_TRAVERSAL, ATTACHMENT_STATUS_RECORD_MISMATCH,
    ATTACHMENT_STATUS_SOURCE_MISSING, ATTACHMENT_STATUS_UNAUTHORIZED,
    ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
};
use agent_memory::attachment::{capture_attachment, AttachmentRequest};
use agent_memory::capture::capture_source;
use agent_memory::model::ContentRef;
use serde_json::{json, Value};

const DOMAIN: &str = "default";

fn make_root() -> (tempfile::TempDir, Archive) {
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(temp.path()).unwrap();
    (temp, archive)
}

fn write_bytes(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, bytes).unwrap();
    path
}

fn png_bytes() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    for index in 0..4096u32 {
        bytes.push((index % 251) as u8);
    }
    bytes
}

/// Write compact JSONL records and return the file plus each record's
/// `(offset, content_len)` (the newline is not part of a record).
fn jsonl_records(dir: &Path, name: &str, values: &[Value]) -> (PathBuf, Vec<(u64, u64)>) {
    let mut text = String::new();
    let mut spans = Vec::new();
    for value in values {
        let line = serde_json::to_string(value).unwrap();
        spans.push((text.len() as u64, line.len() as u64));
        text.push_str(&line);
        text.push('\n');
    }
    let path = write_bytes(dir, name, text.as_bytes());
    (path, spans)
}

fn request(content_ref: ContentRef, source: &Path, roots: &[&Path], target: &str) -> AttachmentRequest {
    AttachmentRequest {
        provider: "claude".to_string(),
        source: source.to_string_lossy().to_string(),
        content_ref,
        target: target.to_string(),
        roots: roots.iter().map(|root| root.to_string_lossy().to_string()).collect(),
        access: vec![DOMAIN.to_string()],
        max_bytes: None,
        record_sha256: None,
        media_type: None,
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(first >> 2) as usize] as char);
        out.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(third & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn object_path(root: &Path, sha: &str) -> PathBuf {
    let dir = root
        .join("archive")
        .join("objects")
        .join(&sha[0..2])
        .join(&sha[2..4]);
    // The storage encoding is explicit in the file name (S2.5).
    let zstd = dir.join(format!("{sha}.zst"));
    if zstd.exists() {
        zstd
    } else {
        dir.join(sha)
    }
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn read_line(path: &Path, index: usize) -> Vec<u8> {
    let raw = fs::read(path).unwrap();
    raw.split(|byte| *byte == b'\n')
        .nth(index)
        .unwrap()
        .to_vec()
}

fn coverage_row<'a>(
    report: &'a agent_memory::coverage::CoverageReport,
    source: &str,
) -> &'a agent_memory::coverage::SourceCoverage {
    report
        .sources
        .iter()
        .find(|row| row.source == source)
        .unwrap_or_else(|| panic!("no coverage row for {source}: {:?}", report.sources))
}

// ---------------------------------------------------------------------------
// 1. Local image: content-addressed binary object + verbatim mapping + readback
// ---------------------------------------------------------------------------
#[test]
fn local_image_is_archived_and_reads_back_byte_exact() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{\"type\":\"summary\"}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let image = png_bytes();
    write_bytes(&attachments, "shot.png", &image);

    let req = request(
        ContentRef::Sidecar {
            rel_path: "shot.png".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    let report = capture_attachment(&archive, &req).unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.status, ATTACHMENT_STATUS_ARCHIVED);
    assert_eq!(report.media_type.as_deref(), Some("image/png"));
    assert_eq!(report.read_mode.as_deref(), Some("file"));
    assert_eq!(report.bytes, image.len() as u64);
    assert_eq!(report.sha256.as_deref(), Some(hex_sha256(&image).as_str()));
    assert!(!report.deduplicated);
    assert_eq!(report.access, vec![DOMAIN.to_string()]);
    assert!(report
        .resolved_path
        .as_deref()
        .unwrap()
        .ends_with("shot.png"));

    let capture_id = report.capture_id.clone().unwrap();
    let attachment_id_value = report.attachment_id.clone().unwrap();
    assert_eq!(attachment_id_value, attachment_id(&capture_id, 0));

    // Byte-exact readback through the fail-closed handle.
    let back = archive
        .reader(DOMAIN)
        .read_attachment(&capture_id, 0)
        .unwrap();
    assert_eq!(back, image);

    // The mapping is committed as a capture fact, verbatim ContentRef included.
    let manifests = archive.manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].attachments.len(), 1);
    assert_eq!(manifests[0].attachments[0].content_ref, req.content_ref);
    assert_eq!(manifests[0].attachments[0].status, ATTACHMENT_STATUS_ARCHIVED);
    assert_eq!(
        manifests[0].attachments[0].object_sha256.as_deref(),
        Some(hex_sha256(&image).as_str())
    );

    // Listing projects the same mapping.
    let entries = archive.attachment_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].attachment_id, attachment_id_value);
    assert!(entries[0].readable);

    // Fail-closed across access domains.
    assert!(matches!(
        archive.reader("other").read_attachment(&capture_id, 0),
        Err(ArchiveError::AccessDenied(_))
    ));
    assert!(matches!(
        archive.reader("").read_attachment(&capture_id, 0),
        Err(ArchiveError::AccessDenied(_))
    ));
}

// ---------------------------------------------------------------------------
// 2. Over-long inline tool output: ContentRef pointer -> object, byte-exact
// ---------------------------------------------------------------------------
#[test]
fn over_long_tool_output_content_ref_is_archived_byte_exact() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let output = format!("begin\r\n中文 needle {} end", "x".repeat(150_000));
    let values = vec![json!({
        "timestamp": "2026-10-26T00:00:00Z",
        "type": "response_item",
        "payload": {"type": "function_call_output", "output": output}
    })];
    let (session, spans) = jsonl_records(dir, "rollout-output.jsonl", &values);
    let (offset, len) = spans[0];

    // The session evidence itself is independent of the attachment archive.
    let capture = capture_source(&archive, &session, "codex", DOMAIN, None).unwrap();
    assert_eq!(capture.status, "captured");
    let record = read_line(&session, 0);
    let record_sha = hex_sha256(&record);
    assert_eq!(
        archive.reader(DOMAIN).read_record(&record_sha).unwrap(),
        record
    );

    let mut req = request(
        ContentRef::Jsonl {
            offset,
            len: u32::try_from(len).unwrap(),
            pointer: "/payload/output".to_string(),
        },
        &session,
        &[],
        "content",
    );
    req.provider = "codex".to_string();
    req.record_sha256 = Some(record_sha.clone());
    let report = capture_attachment(&archive, &req).unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.read_mode.as_deref(), Some("content"));
    assert_eq!(report.bytes, output.len() as u64);
    let capture_id = report.capture_id.clone().unwrap();
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&capture_id, 0)
            .unwrap(),
        output.as_bytes()
    );

    // A second identical capture is a content-addressed dedupe hit, not a
    // second physical copy.
    let again = capture_attachment(&archive, &req).unwrap();
    assert!(again.ok);
    assert!(again.deduplicated);
    assert_eq!(again.sha256, report.sha256);
    assert_eq!(archive.manifests().unwrap().len(), 3, "capture + 2 attachment manifests");
    assert_eq!(
        archive.reconcile(&[]).unwrap().objects_on_disk,
        2,
        "session object + one attachment object"
    );
}

// ---------------------------------------------------------------------------
// 3. Persisted tool output file survives deletion of the source
// ---------------------------------------------------------------------------
#[test]
fn persisted_output_survives_source_deletion() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{\"type\":\"summary\"}\n");
    let results = dir.join("tool-results");
    fs::create_dir_all(&results).unwrap();
    let payload = format!("line one\r\nline two\r\n{}", "z".repeat(300_000)).into_bytes();
    let output = write_bytes(&results, "toolu_01.txt", &payload);

    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "toolu_01.txt".to_string(),
            },
            &session,
            &[&results],
            "local_file",
        ),
    )
    .unwrap();
    assert!(report.ok, "report: {report:?}");
    let capture_id = report.capture_id.clone().unwrap();
    assert_eq!(report.bytes, payload.len() as u64);

    // Delete both the persisted output and its session source.
    fs::remove_file(&output).unwrap();
    fs::remove_file(&session).unwrap();

    let back = archive
        .reader(DOMAIN)
        .read_attachment(&capture_id, 0)
        .unwrap();
    assert_eq!(back, payload, "source deletion must not remove evidence");

    let entries = archive.attachment_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].readable, "archived object still on disk");
    assert_eq!(entries[0].bytes, payload.len() as u64);
}

// ---------------------------------------------------------------------------
// 4. SQLite ContentRef -> archived column value; source stays read-only
// ---------------------------------------------------------------------------
#[test]
fn sqlite_content_ref_is_archived_and_source_is_unchanged() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let db_dir = dir.join("opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let db = db_dir.join("opencode.db");
    let output = format!("sqlite output\r\n{}", "q".repeat(120_000));
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE part (id TEXT PRIMARY KEY, data TEXT);
                 CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT);",
            )
            .unwrap();
        let data = json!({
            "type": "tool",
            "tool": "bash",
            "state": {"input": {"cmd": "echo"}, "output": output}
        });
        connection
            .execute(
                "INSERT INTO part (id, data) VALUES (?1, ?2)",
                rusqlite::params!["prt_1", data.to_string()],
            )
            .unwrap();
    }
    let before = fs::read(&db).unwrap();

    let mut req = request(
        ContentRef::Sqlite {
            table: "part".to_string(),
            id: "prt_1".to_string(),
            column: "data".to_string(),
            pointer: "/state/output".to_string(),
        },
        &db,
        &[],
        "content",
    );
    req.provider = "opencode".to_string();
    let report = capture_attachment(&archive, &req).unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.read_mode.as_deref(), Some("sqlite_text"));
    assert_eq!(report.bytes, output.len() as u64);
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        output.as_bytes()
    );

    // Read-only source: bytes and file set are unchanged (no -wal/-journal).
    assert_eq!(fs::read(&db).unwrap(), before);
    let mut names: Vec<String> = fs::read_dir(&db_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["opencode.db".to_string()]);
}

// ---------------------------------------------------------------------------
// 5. Missing attachment: explicit status + coverage anomaly, evidence intact
// ---------------------------------------------------------------------------
#[test]
fn missing_attachment_is_explicit_and_does_not_affect_other_evidence() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let values = vec![json!({
        "timestamp": "2026-10-26T00:00:00Z",
        "type": "response_item",
        "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}
    })];
    let (session, _) = jsonl_records(dir, "rollout-missing.jsonl", &values);
    let capture = capture_source(&archive, &session, "codex", DOMAIN, None).unwrap();
    assert!(capture.ok);
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();

    let mut req = request(
        ContentRef::Sidecar {
            rel_path: "never-written.png".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    req.provider = "codex".to_string();
    let report = capture_attachment(&archive, &req).unwrap();
    assert!(!report.ok);
    assert_eq!(report.status, ATTACHMENT_STATUS_MISSING);
    assert_eq!(report.bytes, 0);
    assert!(report.sha256.is_none());
    assert!(report.error.as_deref().unwrap().contains("does not exist"));
    assert_eq!(report.anomalies.len(), 1);
    assert!(report.anomalies[0].starts_with("missing:"));

    // The session record is still readable: a missing attachment never takes
    // other evidence with it.
    let record = read_line(&session, 0);
    assert_eq!(
        archive.reader(DOMAIN).read_record(&hex_sha256(&record)).unwrap(),
        record
    );

    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    let row = coverage_row(&coverage, &report.source);
    assert_eq!(row.status, "captured", "source capture health is untouched");
    let attachments_cov = row.attachments.as_ref().expect("attachment coverage");
    assert_eq!(attachments_cov.status, "failed");
    assert_eq!(attachments_cov.last_run.missing, 1);
    assert_eq!(attachments_cov.total.missing, 1);
    assert_eq!(coverage.totals.attachments.missing, 1);
    assert!(coverage
        .anomalies
        .iter()
        .any(|anomaly| anomaly.contains("missing:")));
}

// ---------------------------------------------------------------------------
// 6. Relative path traversal is rejected and never opens the target
// ---------------------------------------------------------------------------
#[test]
fn relative_path_traversal_is_rejected() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let secret = b"TOPSECRET-outside-root";
    write_bytes(dir, "secret.txt", secret);

    for rel_path in ["../secret.txt", "sub/../../secret.txt"] {
        let report = capture_attachment(
            &archive,
            &request(
                ContentRef::Sidecar {
                    rel_path: rel_path.to_string(),
                },
                &session,
                &[&attachments],
                "local_file",
            ),
        )
        .unwrap();
        assert_eq!(report.status, ATTACHMENT_STATUS_PATH_TRAVERSAL, "{rel_path}");
        assert!(!report.ok);
        assert!(report.sha256.is_none());
    }
    assert!(!archive.has_object(&hex_sha256(secret)));
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 0);
    // The traversal attempt did not need the target to exist either.
    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "../does-not-exist".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert_eq!(report.status, ATTACHMENT_STATUS_PATH_TRAVERSAL);
}

// ---------------------------------------------------------------------------
// 7. Absolute path boundary: outside rejected, inside accepted
// ---------------------------------------------------------------------------
#[test]
fn absolute_path_out_of_bounds_is_rejected() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let outside_bytes = b"OUTSIDE";
    let outside = write_bytes(dir, "outside.bin", outside_bytes);
    let inside_bytes = b"INSIDE";
    let inside = write_bytes(&attachments, "inside.bin", inside_bytes);

    let rejected = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: outside.to_string_lossy().to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert_eq!(rejected.status, ATTACHMENT_STATUS_OUT_OF_BOUNDS);
    assert!(!archive.has_object(&hex_sha256(outside_bytes)));

    let accepted = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: inside.to_string_lossy().to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(accepted.ok, "report: {accepted:?}");
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&accepted.capture_id.clone().unwrap(), 0)
            .unwrap(),
        inside_bytes
    );

    // The root boundary is case-insensitive on Windows (one physical path, one
    // boundary, regardless of how the caller spells the drive/directory case).
    #[cfg(windows)]
    {
        let upper = PathBuf::from(attachments.to_string_lossy().to_uppercase());
        let report = capture_attachment(
            &archive,
            &request(
                ContentRef::Sidecar {
                    rel_path: "inside.bin".to_string(),
                },
                &session,
                &[&upper],
                "local_file",
            ),
        )
        .unwrap();
        assert!(report.ok, "case-insensitive root: {report:?}");
        assert!(report.deduplicated, "same bytes -> same object");
    }
}

// ---------------------------------------------------------------------------
// 8. Missing access mapping / unauthorized request fails closed
// ---------------------------------------------------------------------------
#[test]
fn access_mapping_is_required_and_fail_closed() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let payload = png_bytes();
    write_bytes(&attachments, "image.png", &payload);

    let mut req = request(
        ContentRef::Sidecar {
            rel_path: "image.png".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    req.access = Vec::new();
    let unauthorized = capture_attachment(&archive, &req).unwrap();
    assert_eq!(unauthorized.status, ATTACHMENT_STATUS_UNAUTHORIZED);
    assert!(!unauthorized.ok);
    assert!(unauthorized.sha256.is_none());
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 0);
    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert_eq!(coverage.totals.attachments.unauthorized, 1);

    // An archived mapping whose access tags are lost denies every domain.
    req.access = vec![DOMAIN.to_string()];
    let archived = capture_attachment(&archive, &req).unwrap();
    assert!(archived.ok);
    let capture_id = archived.capture_id.clone().unwrap();
    assert!(archive.reader(DOMAIN).read_attachment(&capture_id, 0).is_ok());

    let manifest_path = {
        let dir = archive.root().join("archive").join("manifests");
        let mut names: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        names.sort();
        names.last().unwrap().clone()
    };
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["attachments"][0]["access"] = json!([]);
    manifest["objects"][0]["access"] = json!([]);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    assert!(matches!(
        archive.reader(DOMAIN).read_attachment(&capture_id, 0),
        Err(ArchiveError::AccessDenied(_))
    ));
    let sha = archived.sha256.clone().unwrap();
    assert!(matches!(
        archive.reader(DOMAIN).read_object(&sha),
        Err(ArchiveError::AccessDenied(_))
    ));
}

// ---------------------------------------------------------------------------
// 9. Over-limit objects are explicit and write no object
// ---------------------------------------------------------------------------
#[test]
fn over_limit_attachment_writes_no_object() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let payload = vec![b'a'; 8192];
    let file = write_bytes(&attachments, "big.bin", &payload);

    let mut req = request(
        ContentRef::Sidecar {
            rel_path: "big.bin".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    req.max_bytes = Some(4096);
    let report = capture_attachment(&archive, &req).unwrap();
    assert_eq!(report.status, ATTACHMENT_STATUS_OVER_LIMIT);
    assert_eq!(report.bytes, 0);
    assert_eq!(report.observed_bytes, 8192);
    assert!(report.sha256.is_none());
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 0);
    // The source was not truncated.
    assert_eq!(fs::read(&file).unwrap(), payload);
    assert_eq!(archive.coverage().unwrap().totals.attachments.over_limit, 1);

    // The same limit applies to in-source content.
    let output = "y".repeat(8000);
    let (session, spans) = jsonl_records(
        dir,
        "rollout-big-output.jsonl",
        &[json!({"payload": {"output": output}})],
    );
    let mut req = request(
        ContentRef::Jsonl {
            offset: spans[0].0,
            len: u32::try_from(spans[0].1).unwrap(),
            pointer: "/payload/output".to_string(),
        },
        &session,
        &[],
        "content",
    );
    req.max_bytes = Some(4096);
    let report = capture_attachment(&archive, &req).unwrap();
    assert_eq!(report.status, ATTACHMENT_STATUS_OVER_LIMIT);
    assert_eq!(report.observed_bytes, 8000);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 0);
}

// ---------------------------------------------------------------------------
// 10. Chinese file name, nested sidecar dir and CRLF bytes are preserved
// ---------------------------------------------------------------------------
#[test]
fn chinese_filename_and_crlf_bytes_are_preserved() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("附件");
    fs::create_dir_all(&attachments).unwrap();
    let mut payload = b"\x89PNG\r\n\x1a\n".to_vec();
    payload.extend_from_slice("第一行\r\n第二行 needle\r\n".as_bytes());
    payload.extend("中文内容".repeat(100).as_bytes());
    write_bytes(&attachments, "截图-中文.png", &payload);

    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "截图-中文.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.media_type.as_deref(), Some("image/png"));
    assert!(report
        .resolved_path
        .as_deref()
        .unwrap()
        .ends_with("截图-中文.png"));
    let back = archive
        .reader(DOMAIN)
        .read_attachment(&report.capture_id.clone().unwrap(), 0)
        .unwrap();
    assert_eq!(back, payload);
    assert!(back.windows(2).any(|pair| pair == b"\r\n"));
}

// ---------------------------------------------------------------------------
// 11. Identical content is one physical object, two references
// ---------------------------------------------------------------------------
#[test]
fn duplicate_content_is_one_object_with_two_references() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let payload = png_bytes();
    write_bytes(&attachments, "copy-a.png", &payload);
    write_bytes(&attachments, "copy-b.png", &payload);

    let first = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "copy-a.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    let second = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "copy-b.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(first.ok && second.ok);
    assert!(!first.deduplicated);
    assert!(second.deduplicated);
    assert_eq!(first.sha256, second.sha256);
    assert_eq!(archive.manifests().unwrap().len(), 2);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);

    let entries = archive.attachment_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].sha256, entries[1].sha256);

    let coverage = archive.coverage().unwrap();
    assert_eq!(coverage.totals.attachments.refs, 2);
    assert_eq!(coverage.totals.attachments.archived, 2);
    assert_eq!(coverage.totals.attachments.deduplicated, 1);
    assert_eq!(
        coverage.totals.attachment_bytes_total,
        payload.len() as u64,
        "dedupe-aware storage bytes"
    );
}

// ---------------------------------------------------------------------------
// 12. Second instance is refused by the single-writer lock
// ---------------------------------------------------------------------------
#[test]
fn second_attachment_instance_is_refused() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    write_bytes(&attachments, "lock.png", &png_bytes());
    let req = request(
        ContentRef::Sidecar {
            rel_path: "lock.png".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );

    let guard = archive.acquire_lock().unwrap();
    let error = capture_attachment(&archive, &req).unwrap_err();
    assert!(matches!(error, ArchiveError::LockHeld(_)));
    drop(guard);
    let report = capture_attachment(&archive, &req).unwrap();
    assert!(report.ok, "report: {report:?}");
}

// ---------------------------------------------------------------------------
// 13. Cross-domain dedupe never shares a readback handle
// ---------------------------------------------------------------------------
#[test]
fn cross_domain_dedupe_does_not_share_readback() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    write_bytes(&attachments, "shared.png", &png_bytes());
    write_bytes(&attachments, "only-a.png", b"\x89PNG\r\n\x1a\nonly-a");

    let mut req_a = request(
        ContentRef::Sidecar {
            rel_path: "shared.png".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    req_a.access = vec!["a".to_string()];
    let a = capture_attachment(&archive, &req_a).unwrap();

    let mut req_b = request(
        ContentRef::Sidecar {
            rel_path: "shared.png".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    req_b.access = vec!["b".to_string()];
    let b = capture_attachment(&archive, &req_b).unwrap();
    assert!(b.deduplicated);
    assert_eq!(a.sha256, b.sha256);

    // Strictest intersection: the shared object is visible to neither domain.
    assert!(matches!(
        archive.reader("a").read_attachment(&a.capture_id.clone().unwrap(), 0),
        Err(ArchiveError::AccessDenied(_))
    ));
    assert!(matches!(
        archive.reader("b").read_attachment(&b.capture_id.clone().unwrap(), 0),
        Err(ArchiveError::AccessDenied(_))
    ));

    // A single-domain object stays readable for its own domain only.
    let mut req_only_a = request(
        ContentRef::Sidecar {
            rel_path: "only-a.png".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    req_only_a.access = vec!["a".to_string()];
    let only_a = capture_attachment(&archive, &req_only_a).unwrap();
    assert!(archive
        .reader("a")
        .read_attachment(&only_a.capture_id.clone().unwrap(), 0)
        .is_ok());
    assert!(matches!(
        archive.reader("b").read_attachment(&only_a.capture_id.clone().unwrap(), 0),
        Err(ArchiveError::AccessDenied(_))
    ));
}

// ---------------------------------------------------------------------------
// 14. External URLs are recorded and never fetched
// ---------------------------------------------------------------------------
#[test]
fn external_url_is_never_fetched() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let values = vec![json!({
        "payload": {"output": "https://example.invalid/report.txt", "image": "http://example.invalid/x.png"}
    })];
    let (session, spans) = jsonl_records(dir, "rollout-url.jsonl", &values);
    let source_before = fs::read(&session).unwrap();
    let authorized = dir.join("authorized");
    fs::create_dir_all(&authorized).unwrap();

    for (pointer, target) in [("/payload/output", "local_file"), ("/payload/image", "content")] {
        let report = capture_attachment(
            &archive,
            &request(
                ContentRef::Jsonl {
                    offset: spans[0].0,
                    len: u32::try_from(spans[0].1).unwrap(),
                    pointer: pointer.to_string(),
                },
                &session,
                &[&authorized],
                target,
            ),
        )
        .unwrap();
        assert_eq!(report.status, ATTACHMENT_STATUS_EXTERNAL_REFERENCE, "{pointer}");
        assert!(report.sha256.is_none());
        assert!(report.error.as_deref().unwrap().contains("never fetched"));
    }
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 0);
    // The source is untouched.
    assert_eq!(fs::read(&session).unwrap(), source_before);
}

// ---------------------------------------------------------------------------
// 15. Inline base64 image data URI is decoded and archived as binary content
// ---------------------------------------------------------------------------
#[test]
fn inline_data_uri_image_is_decoded() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let image = png_bytes();
    let data_uri = format!("data:image/png;base64,{}", base64(&image));
    let (session, spans) = jsonl_records(
        dir,
        "claude-inline.jsonl",
        &[json!({"message": {"content": [{"type": "image", "source": {"type": "base64", "data": data_uri}}]}})],
    );

    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Jsonl {
                offset: spans[0].0,
                len: u32::try_from(spans[0].1).unwrap(),
                pointer: "/message/content/0/source/data".to_string(),
            },
            &session,
            &[],
            "content",
        ),
    )
    .unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.read_mode.as_deref(), Some("inline_base64"));
    assert_eq!(report.media_type.as_deref(), Some("image/png"));
    assert_eq!(report.bytes, image.len() as u64);
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        image
    );

    // A data URI is content, not a path.
    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Jsonl {
                offset: spans[0].0,
                len: u32::try_from(spans[0].1).unwrap(),
                pointer: "/message/content/0/source/data".to_string(),
            },
            &session,
            &[dir],
            "local_file",
        ),
    )
    .unwrap();
    assert_eq!(report.status, ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE);
}

// ---------------------------------------------------------------------------
// 16. Tampered object bytes are detected on readback
// ---------------------------------------------------------------------------
#[test]
fn tampered_attachment_object_is_detected() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let payload = png_bytes();
    write_bytes(&attachments, "tamper.png", &payload);

    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "tamper.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(report.ok);
    let sha = report.sha256.clone().unwrap();
    let path = object_path(archive.root(), &sha);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).unwrap();

    assert!(matches!(
        archive
            .reader(DOMAIN)
            .read_attachment(&report.capture_id.clone().unwrap(), 0),
        Err(ArchiveError::Integrity(_))
    ));
    assert!(archive.verify_object_stream(&sha).is_err());
}

// ---------------------------------------------------------------------------
// 17. Large attachment is streamed in chunks and verified
// ---------------------------------------------------------------------------
#[test]
fn large_attachment_is_streamed_and_verified() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let payload: Vec<u8> = (0..4 * 1024 * 1024u32)
        .map(|index| (index % 256) as u8)
        .collect();
    write_bytes(&attachments, "large.bin", &payload);

    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "large.bin".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.bytes, payload.len() as u64);
    assert_eq!(report.sha256.as_deref(), Some(hex_sha256(&payload).as_str()));
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        payload
    );
}

// ---------------------------------------------------------------------------
// 18. Coverage counts a mixed run and current health clears after success
// ---------------------------------------------------------------------------
#[test]
fn coverage_counts_mixed_runs_and_clears_current_health() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    write_bytes(&attachments, "ok.png", &png_bytes());
    write_bytes(&attachments, "too-big.bin", &vec![b'x'; 8192]);

    let archived = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "ok.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(archived.ok);

    let missing = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "gone.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert_eq!(missing.status, ATTACHMENT_STATUS_MISSING);

    let mut over = request(
        ContentRef::Sidecar {
            rel_path: "too-big.bin".to_string(),
        },
        &session,
        &[&attachments],
        "local_file",
    );
    over.max_bytes = Some(1024);
    let over = capture_attachment(&archive, &over).unwrap();
    assert_eq!(over.status, ATTACHMENT_STATUS_OVER_LIMIT);

    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    let row = coverage_row(&coverage, &archived.source);
    let cov = row.attachments.as_ref().unwrap();
    assert_eq!(cov.total.refs, 3);
    assert_eq!(cov.total.archived, 1);
    assert_eq!(cov.total.missing, 1);
    assert_eq!(cov.total.over_limit, 1);
    assert_eq!(cov.status, "failed");
    assert_eq!(cov.last_run.refs, 1);
    assert_eq!(cov.last_run.over_limit, 1);

    // A subsequent successful run clears current health; lifetime counters stay.
    let recovered = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "ok.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(recovered.ok);
    let coverage = archive.coverage().unwrap();
    assert!(coverage.ok, "only the last run decides current health");
    let cov = coverage_row(&coverage, &recovered.source)
        .attachments
        .as_ref()
        .unwrap();
    assert_eq!(cov.total.missing, 1, "history is not rewritten");
    assert_eq!(cov.total.over_limit, 1);
    assert_eq!(cov.status, "archived");
    assert!(cov.anomalies.is_empty());
}

// ---------------------------------------------------------------------------
// 19. In-source reference with a deleted source is explicit, evidence readable
// ---------------------------------------------------------------------------
#[test]
fn in_source_reference_with_missing_source_is_explicit() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let (session, spans) = jsonl_records(
        dir,
        "rollout-deleted.jsonl",
        &[json!({"payload": {"output": "still here"}})],
    );
    let capture = capture_source(&archive, &session, "codex", DOMAIN, None).unwrap();
    assert!(capture.ok);
    let record = read_line(&session, 0);

    let mut req = request(
        ContentRef::Jsonl {
            offset: spans[0].0,
            len: u32::try_from(spans[0].1).unwrap(),
            pointer: "/payload/output".to_string(),
        },
        &session,
        &[],
        "content",
    );
    req.provider = "codex".to_string();
    let archived = capture_attachment(&archive, &req).unwrap();
    assert!(archived.ok, "report: {archived:?}");
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&archived.capture_id.clone().unwrap(), 0)
            .unwrap(),
        b"still here"
    );

    fs::remove_file(&session).unwrap();

    // A new attempt against the deleted source is explicit, not a silent skip.
    let report = capture_attachment(&archive, &req).unwrap();
    assert_eq!(report.status, ATTACHMENT_STATUS_SOURCE_MISSING);
    assert!(report.sha256.is_none());

    // The archived content and the already captured record survive the deletion.
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&archived.capture_id.clone().unwrap(), 0)
            .unwrap(),
        b"still here"
    );
    assert_eq!(
        archive.reader(DOMAIN).read_record(&hex_sha256(&record)).unwrap(),
        record
    );
    assert_eq!(
        archive.coverage().unwrap().totals.attachments.source_missing,
        1
    );
}

// ---------------------------------------------------------------------------
// 20. Record fingerprint mismatch archives nothing
// ---------------------------------------------------------------------------
#[test]
fn record_mismatch_is_explicit() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let (session, spans) = jsonl_records(
        dir,
        "rollout-mismatch.jsonl",
        &[json!({"payload": {"output": "value"}})],
    );
    let mut req = request(
        ContentRef::Jsonl {
            offset: spans[0].0,
            len: u32::try_from(spans[0].1).unwrap(),
            pointer: "/payload/output".to_string(),
        },
        &session,
        &[],
        "content",
    );
    req.record_sha256 = Some("0".repeat(64));
    let report = capture_attachment(&archive, &req).unwrap();
    assert_eq!(report.status, ATTACHMENT_STATUS_RECORD_MISMATCH);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 0);

    // `recordSha256` is a JSONL-only fact; using it elsewhere is a usage error,
    // not a silently ignored field.
    let mut sidecar = request(
        ContentRef::Sidecar {
            rel_path: "whatever.png".to_string(),
        },
        &session,
        &[dir],
        "local_file",
    );
    sidecar.record_sha256 = Some("0".repeat(64));
    assert!(matches!(
        capture_attachment(&archive, &sidecar),
        Err(ArchiveError::Corrupt(_))
    ));

    // Usage errors are explicit errors, never a silent partial archive.
    let mut bogus_target = request(
        ContentRef::Sidecar {
            rel_path: "whatever.png".to_string(),
        },
        &session,
        &[dir],
        "local_file",
    );
    bogus_target.target = "bogus".to_string();
    assert!(matches!(
        capture_attachment(&archive, &bogus_target),
        Err(ArchiveError::Corrupt(_))
    ));

    let mut bogus_provider = request(
        ContentRef::Sidecar {
            rel_path: "whatever.png".to_string(),
        },
        &session,
        &[dir],
        "local_file",
    );
    bogus_provider.provider = "not-a-provider".to_string();
    assert!(matches!(
        capture_attachment(&archive, &bogus_provider),
        Err(ArchiveError::Corrupt(_))
    ));

    let mut relative_source = request(
        ContentRef::Sidecar {
            rel_path: "whatever.png".to_string(),
        },
        &session,
        &[dir],
        "local_file",
    );
    relative_source.source = "relative/session.jsonl".to_string();
    assert!(matches!(
        capture_attachment(&archive, &relative_source),
        Err(ArchiveError::Corrupt(_))
    ));

    let mut relative_root = request(
        ContentRef::Sidecar {
            rel_path: "whatever.png".to_string(),
        },
        &session,
        &[dir],
        "local_file",
    );
    relative_root.roots = vec!["relative/root".to_string()];
    assert!(matches!(
        capture_attachment(&archive, &relative_root),
        Err(ArchiveError::Corrupt(_))
    ));
}

// ---------------------------------------------------------------------------
// 21. Unsupported references are explicit (bad pointers, schema, ranges)
// ---------------------------------------------------------------------------
#[test]
fn unsupported_references_are_explicit() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let (session, spans) = jsonl_records(
        dir,
        "rollout-unsupported.jsonl",
        &[json!({"payload": {"output": "text", "count": 7}})],
    );
    let db = dir.join("opencode.db");
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch("CREATE TABLE part (id TEXT PRIMARY KEY, data TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO part (id, data) VALUES (?1, ?2)",
                rusqlite::params!["prt_1", "{\"state\":{\"output\":\"x\"}}"],
            )
            .unwrap();
    }

    let cases: Vec<(ContentRef, PathBuf, &str)> = vec![
        // Pointer that does not exist.
        (
            ContentRef::Jsonl {
                offset: spans[0].0,
                len: u32::try_from(spans[0].1).unwrap(),
                pointer: "/payload/nope".to_string(),
            },
            session.clone(),
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
        ),
        // Pointer into a number (not a string value).
        (
            ContentRef::Jsonl {
                offset: spans[0].0,
                len: u32::try_from(spans[0].1).unwrap(),
                pointer: "/payload/count".to_string(),
            },
            session.clone(),
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
        ),
        // Pointer without the leading slash.
        (
            ContentRef::Jsonl {
                offset: spans[0].0,
                len: u32::try_from(spans[0].1).unwrap(),
                pointer: "payload/output".to_string(),
            },
            session.clone(),
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
        ),
        // Range beyond the source length.
        (
            ContentRef::Jsonl {
                offset: 0,
                len: 1_000_000,
                pointer: "/payload/output".to_string(),
            },
            session.clone(),
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
        ),
        // Unknown SQLite table.
        (
            ContentRef::Sqlite {
                table: "missing_table".to_string(),
                id: "x".to_string(),
                column: "data".to_string(),
                pointer: String::new(),
            },
            db.clone(),
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
        ),
        // Unsafe SQL identifier.
        (
            ContentRef::Sqlite {
                table: "part; DROP TABLE part".to_string(),
                id: "prt_1".to_string(),
                column: "data".to_string(),
                pointer: String::new(),
            },
            db.clone(),
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
        ),
        // Unknown SQLite row.
        (
            ContentRef::Sqlite {
                table: "part".to_string(),
                id: "no-such-row".to_string(),
                column: "data".to_string(),
                pointer: String::new(),
            },
            db.clone(),
            ATTACHMENT_STATUS_MISSING,
        ),
    ];
    for (content_ref, source, expected) in cases {
        let mut req = request(content_ref, &source, &[], "content");
        req.provider = "opencode".to_string();
        let report = capture_attachment(&archive, &req).unwrap();
        assert!(!report.ok);
        assert_eq!(report.status, expected, "reference: {:?}", req.content_ref);
    }
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 0);
    // The source database is untouched by the rejected reads.
    assert!(fs::read_dir(dir)
        .unwrap()
        .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("-wal")));
}

// ---------------------------------------------------------------------------
// 22. JSON part file reference: in-file pointer, relative media path, boundary
// ---------------------------------------------------------------------------
#[test]
fn json_part_file_reference_resolves_pointer_and_stays_bounded() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let storage = dir.join("storage");
    fs::create_dir_all(storage.join("part")).unwrap();
    fs::create_dir_all(storage.join("media")).unwrap();
    let part = write_bytes(
        &storage,
        "part/p1.json",
        &serde_json::to_vec(&json!({"state": {"output": "part content"}})).unwrap(),
    );
    write_bytes(&storage, "media/a.bin", b"FILEPART");

    // target=content: the pointed value bytes in the part file.
    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::File {
                rel_path: "part/p1.json".to_string(),
                pointer: "/state/output".to_string(),
            },
            &part,
            &[&storage],
            "content",
        ),
    )
    .unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.read_mode.as_deref(), Some("file_json"));
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        b"part content"
    );

    // target=local_file: the pointed string resolves against the authorized
    // root (roots[0]).
    let part_two = write_bytes(
        &storage,
        "part/p2.json",
        &serde_json::to_vec(&json!({"path": "media/a.bin"})).unwrap(),
    );
    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::File {
                rel_path: "part/p2.json".to_string(),
                pointer: "/path".to_string(),
            },
            &part_two,
            &[&storage],
            "local_file",
        ),
    )
    .unwrap();
    assert!(report.ok, "report: {report:?}");
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        b"FILEPART"
    );

    // A path value that escapes the authorized storage root is rejected.
    let part_three = write_bytes(
        &storage,
        "part/p3.json",
        &serde_json::to_vec(&json!({"path": "../outside.bin"})).unwrap(),
    );
    write_bytes(dir, "outside.bin", b"OUTSIDE-PART");
    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::File {
                rel_path: "part/p3.json".to_string(),
                pointer: "/path".to_string(),
            },
            &part_three,
            &[&storage],
            "local_file",
        ),
    )
    .unwrap();
    assert_eq!(report.status, ATTACHMENT_STATUS_PATH_TRAVERSAL);
    assert!(!archive.has_object(&hex_sha256(b"OUTSIDE-PART")));
}

// ---------------------------------------------------------------------------
// 23. CLI JSON contract: attach / attachments / attachment-read
// ---------------------------------------------------------------------------
#[test]
fn cli_attach_and_read_are_json_contract() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let payload = png_bytes();
    write_bytes(&attachments, "cli.png", &payload);

    let request_value = json!({
        "provider": "claude",
        "source": session.to_string_lossy(),
        "contentRef": {"kind": "sidecar", "relPath": "cli.png"},
        "target": "local_file",
        "roots": [attachments.to_string_lossy()],
        "access": [DOMAIN],
    });
    let request_path = write_bytes(
        dir,
        "request.json",
        &serde_json::to_vec_pretty(&request_value).unwrap(),
    );

    let output = cli()
        .arg("attach")
        .arg("--root")
        .arg(archive.root())
        .arg("--request")
        .arg(format!("@{}", request_path.display()))
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "archived");
    assert_eq!(value["ok"], true);
    assert_eq!(value["sha256"], hex_sha256(&payload));
    let capture_id = value["captureId"].as_str().unwrap().to_string();
    assert_eq!(
        value["attachmentId"].as_str().unwrap(),
        attachment_id(&capture_id, 0)
    );

    let list = cli()
        .arg("attachments")
        .arg("--root")
        .arg(archive.root())
        .output()
        .unwrap();
    assert!(list.status.success());
    let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(listed["attachments"][0]["sha256"], hex_sha256(&payload));
    assert_eq!(listed["attachments"][0]["readable"], true);
    assert_eq!(listed["attachments"][0]["status"], "archived");

    let out_path = dir.join("readback.png");
    let read = cli()
        .arg("attachment-read")
        .arg("--root")
        .arg(archive.root())
        .arg("--domain")
        .arg(DOMAIN)
        .arg("--capture-id")
        .arg(&capture_id)
        .arg("--seq")
        .arg("0")
        .arg("--out")
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(
        read.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert_eq!(fs::read(&out_path).unwrap(), payload);
    let read_value: Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read_value["verified"], true);
    assert_eq!(read_value["sha256"], hex_sha256(&payload));

    // Explicit failure exits: missing = 7, unauthorized = 5.
    let missing = json!({
        "provider": "claude",
        "source": session.to_string_lossy(),
        "contentRef": {"kind": "sidecar", "relPath": "gone.bin"},
        "target": "local_file",
        "roots": [attachments.to_string_lossy()],
        "access": [DOMAIN],
    });
    let missing_path = write_bytes(
        dir,
        "missing.json",
        &serde_json::to_vec_pretty(&missing).unwrap(),
    );
    let output = cli()
        .arg("attach")
        .arg("--root")
        .arg(archive.root())
        .arg("--request")
        .arg(format!("@{}", missing_path.display()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "missing");

    let unauthorized = json!({
        "provider": "claude",
        "source": session.to_string_lossy(),
        "contentRef": {"kind": "sidecar", "relPath": "cli.png"},
        "target": "local_file",
        "roots": [attachments.to_string_lossy()],
        "access": [],
    });
    let unauthorized_path = write_bytes(
        dir,
        "unauthorized.json",
        &serde_json::to_vec_pretty(&unauthorized).unwrap(),
    );
    let output = cli()
        .arg("attach")
        .arg("--root")
        .arg(archive.root())
        .arg("--request")
        .arg(format!("@{}", unauthorized_path.display()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
}

// ---------------------------------------------------------------------------
// 24. Lost collector state: the mapping stays visible, health is unknown
// ---------------------------------------------------------------------------
#[test]
fn lost_collector_state_keeps_mappings_visible_and_reports_unknown_health() {
    let (temp, archive) = make_root();
    let dir = temp.path();
    let session = write_bytes(dir, "session.jsonl", b"{}\n");
    let attachments = dir.join("attachments");
    fs::create_dir_all(&attachments).unwrap();
    let payload = png_bytes();
    write_bytes(&attachments, "state-loss.png", &payload);

    let report = capture_attachment(
        &archive,
        &request(
            ContentRef::Sidecar {
                rel_path: "state-loss.png".to_string(),
            },
            &session,
            &[&attachments],
            "local_file",
        ),
    )
    .unwrap();
    assert!(report.ok);
    fs::remove_file(archive.root().join("state").join("sources.json")).unwrap();

    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok, "current health is unknowable without state");
    let row = coverage_row(&coverage, &report.source);
    assert_eq!(row.status, "attachment_only");
    let cov = row.attachments.as_ref().expect("manifest-derived coverage");
    assert_eq!(cov.total.archived, 1);
    assert_eq!(cov.total.refs, 1);
    assert_eq!(cov.status, "none");
    assert!(coverage
        .anomalies
        .iter()
        .any(|anomaly| anomaly.contains("last-run health unknown")));

    // Evidence is independent of collector state.
    assert_eq!(
        archive
            .reader(DOMAIN)
            .read_attachment(&report.capture_id.clone().unwrap(), 0)
            .unwrap(),
        payload
    );
}
