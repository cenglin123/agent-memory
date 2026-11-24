//! P4: attachment and complete-content references.
//!
//! A [`ContentRef`] found in a session record (JSONL line/pointer, SQLite
//! row/column/pointer, JSON sidecar file/pointer, plain sidecar path) is turned
//! into a content-addressed binary object, and the mapping ContentRef -> object
//! is committed as a capture fact in a manifest. The source file is only ever
//! read; the archive copy is what makes the content survive source cleanup.
//!
//! # Two materialization modes
//!
//! - `target = "content"`: archive the bytes the reference *locates*. A JSONL
//!   pointer resolves to a string value (long tool output, inline image text); an
//!   empty JSONL pointer archives the raw record bytes; a SQLite column archives
//!   its text or blob value; an empty `file` pointer archives the part file
//!   itself. A `data:` URI is base64-decoded and archived as binary content.
//! - `target = "local_file"`: resolve the located string as a local path and
//!   archive the file it names (persisted tool output, images, attachments).
//!
//! # Explicit outcomes (never a silent skip)
//!
//! Every attempt commits a manifest attachment entry with one status:
//! `archived`, `missing`, `path_traversal`, `out_of_bounds`, `unauthorized`,
//! `over_limit`, `external_reference`, `unsupported_reference`, `read_failed`,
//! `source_missing`, `record_mismatch`. A rejected reference has no object and
//! keeps its reason; coverage counts it and flips `ok` for that run's health.
//!
//! # Boundaries
//!
//! - `source`, `roots` and any absolute reference must be absolute paths; there
//!   is no current-directory fallback.
//! - Relative references resolve against the **first** authorized root; every
//!   resulting path (lexically normalized, then canonicalized when it exists)
//!   must stay under an authorized root. `..` that leaves the base is
//!   `path_traversal`; an absolute path outside every root is `out_of_bounds`;
//!   neither case ever opens the target file.
//! - `http(s)://` / `ftp(s)://` / `ws(s)://` and any other `scheme://` value is
//!   `external_reference`: it is recorded and never fetched. A `file:` URL is
//!   accepted as a local path (no percent-decoding, no UNC mapping); a `data:`
//!   URI is only accepted for `target = "content"`.
//! - The size limit is enforced while streaming (`Archive::put_object_stream`),
//!   so an over-limit object writes no bytes into the object store.
//! - The runtime-root single-writer lock is held for the whole run; a second
//!   instance is refused with [`ArchiveError::LockHeld`].

use std::fs::{self, File};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::archive::{
    attachment_id, hex_sha256, new_id, now_ms, Archive, ArchiveError, AttachmentCounts,
    AttachmentState, Manifest, ManifestAttachment, ManifestObject, ManifestSource,
    SourceFingerprint, SourceState, StorageHint, ATTACHMENT_SOURCE_KIND,
    ATTACHMENT_STATUS_ARCHIVED, ATTACHMENT_STATUS_EXTERNAL_REFERENCE,
    ATTACHMENT_STATUS_MISSING, ATTACHMENT_STATUS_OUT_OF_BOUNDS,
    ATTACHMENT_STATUS_OVER_LIMIT, ATTACHMENT_STATUS_PATH_TRAVERSAL,
    ATTACHMENT_STATUS_READ_FAILED, ATTACHMENT_STATUS_RECORD_MISMATCH,
    ATTACHMENT_STATUS_SOURCE_MISSING, ATTACHMENT_STATUS_UNAUTHORIZED,
    ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE, MANIFEST_SCHEMA,
};
use crate::capture::canonical_key;
use crate::error::AppError;
use crate::model::ContentRef;
use crate::SUPPORTED_PROVIDERS;

/// Default per-reference size limit (32 MiB). A reference larger than this is
/// `over_limit` and writes no object; callers may raise or lower it per request.
pub const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

/// SQLite busy timeout for read-only reference resolution.
const SQLITE_BUSY_TIMEOUT_MS: u64 = 250;

/// Head bytes read from a file to detect its media type before streaming.
const MEDIA_SNIFF_BYTES: u64 = 512;

// ---------------------------------------------------------------------------
// Request / report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRequest {
    /// Provider that produced the source record (`claude` | `codex` | `opencode`).
    pub provider: String,
    /// Absolute path of the physical session source the reference came from
    /// (JSONL file, SQLite database, or the JSON part file for `file` refs).
    pub source: String,
    /// The original reference, stored verbatim in the manifest.
    pub content_ref: ContentRef,
    /// `content` (default) or `local_file`.
    #[serde(default = "default_target")]
    pub target: String,
    /// Authorized local roots. `roots[0]` is the base for relative references.
    /// Absolute references are accepted only under one of these roots.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Reference-level access tags frozen at capture time. Empty = unauthorized.
    #[serde(default)]
    pub access: Vec<String>,
    /// Overrides [`DEFAULT_MAX_ATTACHMENT_BYTES`]. `0` means "use the default".
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// Optional expected SHA-256 of the JSONL record range; a mismatch is
    /// `record_mismatch` and archives nothing.
    #[serde(default)]
    pub record_sha256: Option<String>,
    /// Declared media type (wins over magic detection, e.g. for a data URI).
    #[serde(default)]
    pub media_type: Option<String>,
}

fn default_target() -> String {
    "content".to_string()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentReport {
    pub ok: bool,
    /// One of the explicit `ATTACHMENT_STATUS_*` values.
    pub status: String,
    pub provider: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
    pub target: String,
    pub content_ref: ContentRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Bytes archived (0 when nothing was archived).
    pub bytes: u64,
    /// Bytes observed at the source (file size or located content length).
    pub observed_bytes: u64,
    pub deduplicated: bool,
    pub access: Vec<String>,
    pub limit_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Last run's explicit anomalies (`<status>:<locator>: <detail>`).
    pub anomalies: Vec<String>,
    #[serde(default)]
    pub orphan_objects: Vec<String>,
}

// ---------------------------------------------------------------------------
// Internal failure / plan types
// ---------------------------------------------------------------------------

struct Failure {
    status: &'static str,
    error: String,
    observed_bytes: u64,
}

impl Failure {
    fn new(status: &'static str, error: impl Into<String>) -> Self {
        Self {
            status,
            error: error.into(),
            observed_bytes: 0,
        }
    }

    fn with_observed(mut self, observed: u64) -> Self {
        self.observed_bytes = observed;
        self
    }
}

enum Located {
    /// A decoded string value at the reference (or a text column).
    Text(String),
    /// Raw bytes: an empty-pointer record, a blob column, or a whole part file.
    Raw(Vec<u8>),
}

enum Plan {
    Content {
        bytes: Vec<u8>,
        read_mode: &'static str,
        media_type: String,
    },
    File {
        path: PathBuf,
        observed_bytes: u64,
    },
    Failure(Failure),
}

enum UriKind {
    File,
    Data,
    External,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Archive one ContentRef target into the evidence store and commit the mapping.
///
/// This is the only write path for attachments; it acquires the runtime-root
/// single-writer lock, never writes to the source, and commits a manifest even
/// when the reference is rejected (so the outcome is evidence, not a log line).
pub fn capture_attachment(
    archive: &Archive,
    request: &AttachmentRequest,
) -> Result<AttachmentReport, ArchiveError> {
    if !SUPPORTED_PROVIDERS.contains(&request.provider.as_str()) {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported provider {:?} (supported: {SUPPORTED_PROVIDERS:?})",
            request.provider
        )));
    }
    if request.target != "content" && request.target != "local_file" {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported target {:?} (content | local_file)",
            request.target
        )));
    }
    if request.record_sha256.is_some()
        && !matches!(request.content_ref, ContentRef::Jsonl { .. })
    {
        return Err(ArchiveError::Corrupt(
            "recordSha256 only applies to jsonl references".to_string(),
        ));
    }
    let source_path = PathBuf::from(&request.source);
    if !source_path.is_absolute() {
        return Err(ArchiveError::Corrupt(
            "source must be an absolute path (no current-directory fallback)".to_string(),
        ));
    }
    let roots = normalize_roots(&request.roots)?;
    let limit = match request.max_bytes {
        Some(0) | None => DEFAULT_MAX_ATTACHMENT_BYTES,
        Some(value) => value,
    };
    let access = normalize_tags(&request.access);

    let _lock = archive.acquire_lock()?;
    let source_key = canonical_key(&source_path);

    let plan = if access.is_empty() {
        Plan::Failure(Failure::new(
            ATTACHMENT_STATUS_UNAUTHORIZED,
            "request carries no access tags (fail-closed)",
        ))
    } else if needs_roots(request) && roots.is_empty() {
        Plan::Failure(Failure::new(
            ATTACHMENT_STATUS_UNAUTHORIZED,
            "no authorized root for path resolution (fail-closed)",
        ))
    } else {
        plan_attachment(request, &source_path, &roots, limit)
    };

    let mut counts = AttachmentCounts::default();
    let mut anomalies: Vec<String> = Vec::new();
    let status: String;
    let mut error: Option<String> = None;
    let mut sha256: Option<String> = None;
    let mut bytes = 0u64;
    let observed_bytes: u64;
    let mut deduplicated = false;
    let mut read_mode = String::new();
    let mut media_type = String::new();
    let mut resolved_path: Option<String> = None;

    match plan {
        Plan::Failure(failure) => {
            status = failure.status.to_string();
            observed_bytes = failure.observed_bytes;
            error = Some(failure.error);
        }
        Plan::Content {
            bytes: content,
            read_mode: mode,
            media_type: detected,
        } => {
            observed_bytes = content.len() as u64;
            if observed_bytes > limit {
                status = ATTACHMENT_STATUS_OVER_LIMIT.to_string();
                error = Some(format!(
                    "located content is {observed_bytes} bytes, over the {limit}-byte limit; \
                     nothing archived"
                ));
            } else {
                let mut cursor = Cursor::new(content);
                let (sha, written, is_new, _) =
                    archive.put_object_stream_encoded(&mut cursor, limit, StorageHint::Compressible)?;
                archive.verify_object_stream(&sha)?;
                status = ATTACHMENT_STATUS_ARCHIVED.to_string();
                sha256 = Some(sha);
                bytes = written;
                deduplicated = !is_new;
                read_mode = mode.to_string();
                media_type = detected;
            }
        }
        Plan::File {
            path,
            observed_bytes: observed,
        } => {
            observed_bytes = observed;
            if observed > limit {
                status = ATTACHMENT_STATUS_OVER_LIMIT.to_string();
                error = Some(format!(
                    "file is {observed} bytes, over the {limit}-byte limit; nothing archived"
                ));
            } else {
                let mut file = File::open(&path).map_err(|e| AppError::io(&path, e))?;
                match archive.put_object_stream_encoded(&mut file, limit, StorageHint::Compressible) {
                    Ok((sha, written, is_new, _)) => {
                        archive.verify_object_stream(&sha)?;
                        status = ATTACHMENT_STATUS_ARCHIVED.to_string();
                        sha256 = Some(sha);
                        bytes = written;
                        deduplicated = !is_new;
                        read_mode = "file".to_string();
                        let head = read_head(&path, MEDIA_SNIFF_BYTES)?;
                        media_type = detect_media_type(&head);
                        resolved_path = Some(path.to_string_lossy().to_string());
                    }
                    Err(ArchiveError::LimitExceeded(limit)) => {
                        status = ATTACHMENT_STATUS_OVER_LIMIT.to_string();
                        error = Some(format!(
                            "file grew past the {limit}-byte limit while streaming; nothing archived"
                        ));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    if let Some(declared) = request.media_type.as_deref() {
        if !declared.trim().is_empty() {
            media_type = declared.trim().to_string();
        }
    }
    if media_type.is_empty() {
        media_type = "application/octet-stream".to_string();
    }

    counts.record(&status);
    if status == ATTACHMENT_STATUS_ARCHIVED && deduplicated {
        counts.deduplicated = counts.deduplicated.saturating_add(1);
    }
    if status != ATTACHMENT_STATUS_ARCHIVED {
        anomalies.push(format!(
            "{status}:{}: {}",
            describe_ref(&request.content_ref),
            error.as_deref().unwrap_or("rejected")
        ));
    }

    // (1) commit point: the mapping is evidence even when no object was written.
    let captured_at = now_ms();
    let (fingerprint, source_state) = source_fingerprint(&source_path)?;
    let entry = ManifestAttachment {
        seq: 0,
        content_ref: request.content_ref.clone(),
        target: request.target.clone(),
        read_mode: read_mode.clone(),
        resolved_path: resolved_path.clone(),
        media_type: media_type.clone(),
        status: status.clone(),
        object_sha256: sha256.clone(),
        bytes,
        observed_bytes,
        access: access.clone(),
        deduplicated,
        limit_bytes: limit,
        error: error.clone(),
    };
    let objects = match &sha256 {
        Some(sha) => vec![ManifestObject {
            sha256: sha.clone(),
            bytes,
            role: "attachment".to_string(),
            access: access.clone(),
            source_offset: None,
            generation: 1,
        }],
        None => Vec::new(),
    };
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_string(),
        capture_id: new_id("att"),
        captured_at_ms: captured_at,
        device_id: archive.device_id()?,
        provider: request.provider.clone(),
        source: ManifestSource {
            kind: ATTACHMENT_SOURCE_KIND.to_string(),
            path: source_key.clone(),
            compressed: false,
            fingerprint,
            source_state_at_capture: source_state,
            capture_reason: "attachment".to_string(),
            generation: 1,
            range_start: 0,
            carrier: None,
            sqlite: None,
        },
        objects,
        records: Vec::new(),
        attachments: vec![entry],
    };
    archive.commit_manifest(&manifest)?;

    // (2) last-run health projection (historical facts stay in the manifests).
    let mut state = archive.load_sources()?;
    let source_entry = state
        .sources
        .entry(source_key.clone())
        .or_insert_with(|| SourceState {
            provider: request.provider.clone(),
            ..SourceState::default()
        });
    if source_entry.provider.is_empty() {
        source_entry.provider = request.provider.clone();
    }
    if source_entry.status.is_empty() {
        // No session capture for this source yet: say so instead of claiming
        // `captured` (attachments are not a source watermark).
        source_entry.status = "attachment_only".to_string();
    }
    let attachment = source_entry
        .attachment
        .get_or_insert_with(AttachmentState::default);
    attachment.last_run_at_ms = captured_at;
    attachment.last_status = if counts.rejected() == 0 {
        "archived".to_string()
    } else if counts.archived > 0 {
        "partial".to_string()
    } else {
        "failed".to_string()
    };
    attachment.last_run = counts.clone();
    attachment.last_limit_bytes = limit;
    attachment.anomalies = anomalies.clone();
    archive.save_sources(&state)?;

    let attachment_id_value = attachment_id(&manifest.capture_id, 0);
    Ok(AttachmentReport {
        ok: status == ATTACHMENT_STATUS_ARCHIVED,
        status,
        provider: request.provider.clone(),
        source: source_key,
        capture_id: Some(manifest.capture_id.clone()),
        attachment_id: Some(attachment_id_value),
        target: request.target.clone(),
        content_ref: request.content_ref.clone(),
        read_mode: if read_mode.is_empty() {
            None
        } else {
            Some(read_mode)
        },
        resolved_path,
        media_type: Some(media_type),
        sha256,
        bytes,
        observed_bytes,
        deduplicated,
        access,
        limit_bytes: limit,
        error,
        anomalies,
        orphan_objects: archive
            .reconcile_after_unit()?
            .map(|reconcile| reconcile.orphan_objects)
            .unwrap_or_default(),
    })
}

fn needs_roots(request: &AttachmentRequest) -> bool {
    request.target == "local_file"
        || matches!(
            request.content_ref,
            ContentRef::Sidecar { .. } | ContentRef::File { .. }
        )
}

// ---------------------------------------------------------------------------
// Reference resolution
// ---------------------------------------------------------------------------

fn plan_attachment(
    request: &AttachmentRequest,
    source: &Path,
    roots: &[PathBuf],
    limit: u64,
) -> Plan {
    match plan_inner(request, source, roots, limit) {
        Ok(plan) => plan,
        Err(failure) => Plan::Failure(failure),
    }
}

fn plan_inner(
    request: &AttachmentRequest,
    source: &Path,
    roots: &[PathBuf],
    limit: u64,
) -> Result<Plan, Failure> {
    let want_file = request.target == "local_file";
    match &request.content_ref {
        ContentRef::Jsonl {
            offset,
            len,
            pointer,
        } => {
            let located = locate_jsonl(
                source,
                *offset,
                *len,
                pointer,
                request.record_sha256.as_deref(),
            )?;
            if want_file {
                let base = roots.first().expect("roots checked by caller");
                file_plan(located, base, roots, limit)
            } else {
                let mode = if pointer.is_empty() { "record" } else { "content" };
                content_plan(located, mode)
            }
        }
        ContentRef::Sqlite {
            table,
            id,
            column,
            pointer,
        } => {
            let located = locate_sqlite(source, table, id, column, pointer)?;
            if want_file {
                let base = roots.first().expect("roots checked by caller");
                file_plan(located, base, roots, limit)
            } else {
                // `content_plan` overwrites the mode for raw blobs; a raw blob
                // stays a blob, text stays a sqlite column value.
                match &located {
                    Located::Raw(_) => content_plan(located, "sqlite_blob"),
                    Located::Text(_) => content_plan(located, "sqlite_text"),
                }
            }
        }
        ContentRef::Sidecar { rel_path } => {
            if !want_file {
                return Err(Failure::new(
                    ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                    "sidecar references denote local files; use target=local_file",
                ));
            }
            let base = roots.first().expect("roots checked by caller");
            // Route through `file_plan` so an external URL or a data URI in a
            // sidecar reference gets its explicit status instead of being read
            // as a filename.
            file_plan(Located::Text(rel_path.clone()), base, roots, limit)
        }
        ContentRef::File { rel_path, pointer } => {
            let base = roots.first().expect("roots checked by caller");
            let json_path = resolve_path_with_base(rel_path, base, roots)?;
            let meta = fs::metadata(&json_path)
                .map_err(|e| Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(&json_path, &e)))?;
            if meta.len() > limit {
                return Err(Failure::new(
                    ATTACHMENT_STATUS_OVER_LIMIT,
                    format!(
                        "JSON part file is {} bytes, over the {limit}-byte limit; part files are \
                         bounded because they are parsed in memory",
                        meta.len()
                    ),
                )
                .with_observed(meta.len()));
            }
            let raw = fs::read(&json_path).map_err(|e| {
                Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(&json_path, &e))
            })?;
            let located = if pointer.is_empty() {
                Located::Raw(raw)
            } else {
                let value: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| {
                    Failure::new(
                        ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                        format!("part file is not valid JSON: {e}"),
                    )
                })?;
                let target = resolve_pointer(&value, pointer)?;
                match target {
                    serde_json::Value::String(text) => Located::Text(text.clone()),
                    other => {
                        return Err(Failure::new(
                            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                            format!(
                                "pointer resolves to {}, not a string",
                                json_kind(other)
                            ),
                        ))
                    }
                }
            };
            if want_file {
                // A path value inside a part file resolves against the
                // authorized root (roots[0]), the same base as any other
                // reference value: one rule, no part-directory special case.
                file_plan(located, base, roots, limit)
            } else {
                content_plan(located, "file_json")
            }
        }
    }
}

/// Archive the bytes a reference locates (`target = "content"`).
fn content_plan(located: Located, read_mode: &'static str) -> Result<Plan, Failure> {
    match located {
        Located::Raw(bytes) => {
            let media_type = detect_media_type(&bytes);
            Ok(Plan::Content {
                bytes,
                read_mode,
                media_type,
            })
        }
        Located::Text(text) => match classify_uri(&text) {
            Some(UriKind::Data) => {
                let (media_type, bytes) = parse_data_uri(&text)?;
                Ok(Plan::Content {
                    bytes,
                    read_mode: "inline_base64",
                    media_type,
                })
            }
            Some(UriKind::External) => Err(Failure::new(
                ATTACHMENT_STATUS_EXTERNAL_REFERENCE,
                "external URL is never fetched; only authorized local sources are archived",
            )),
            Some(UriKind::File) => Err(Failure::new(
                ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                "file: URL denotes a local file; use target=local_file",
            )),
            None => {
                let bytes = text.into_bytes();
                let media_type = detect_media_type(&bytes);
                Ok(Plan::Content {
                    bytes,
                    read_mode,
                    media_type,
                })
            }
        },
    }
}

/// Resolve the located string as a path and archive the file (`target = "local_file"`).
fn file_plan(
    located: Located,
    base: &Path,
    roots: &[PathBuf],
    limit: u64,
) -> Result<Plan, Failure> {
    let value = match located {
        Located::Text(text) => text,
        Located::Raw(_) => {
            return Err(Failure::new(
                ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                "reference resolves to binary content, not a path",
            ))
        }
    };
    let candidate = match classify_uri(&value) {
        Some(UriKind::Data) => {
            return Err(Failure::new(
                ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                "inline data URI is content; use target=content",
            ))
        }
        Some(UriKind::External) => {
            return Err(Failure::new(
                ATTACHMENT_STATUS_EXTERNAL_REFERENCE,
                "external URL is never fetched; only authorized local sources are archived",
            ))
        }
        Some(UriKind::File) => file_url_to_path(&value),
        None => value,
    };
    let path = resolve_path_with_base(&candidate, base, roots)?;
    file_plan_for_path(path, roots, limit)
}

fn file_plan_for_path(path: PathBuf, _roots: &[PathBuf], limit: u64) -> Result<Plan, Failure> {
    let meta = fs::metadata(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Failure::new(
            ATTACHMENT_STATUS_MISSING,
            format!("file does not exist: {}", path.display()),
        ),
        std::io::ErrorKind::PermissionDenied => Failure::new(
            ATTACHMENT_STATUS_READ_FAILED,
            format!("permission denied: {}", path.display()),
        ),
        _ => Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(&path, &e)),
    })?;
    if !meta.is_file() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_READ_FAILED,
            format!("not a regular file: {}", path.display()),
        ));
    }
    let observed = meta.len();
    if observed > limit {
        return Err(Failure::new(
            ATTACHMENT_STATUS_OVER_LIMIT,
            format!(
                "file is {observed} bytes, over the {limit}-byte limit: {}",
                path.display()
            ),
        )
        .with_observed(observed));
    }
    Ok(Plan::File {
        path,
        observed_bytes: observed,
    })
}

fn locate_jsonl(
    source: &Path,
    offset: u64,
    len: u32,
    pointer: &str,
    expected_record_sha: Option<&str>,
) -> Result<Located, Failure> {
    let meta = fs::metadata(source).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Failure::new(
            ATTACHMENT_STATUS_SOURCE_MISSING,
            format!("source does not exist: {}", source.display()),
        ),
        _ => Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(source, &e)),
    })?;
    if !meta.is_file() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_READ_FAILED,
            format!("source is not a regular file: {}", source.display()),
        ));
    }
    let len_usize = usize::try_from(len).map_err(|_| {
        Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "record length does not fit this platform",
        )
    })?;
    let end = offset.checked_add(u64::from(len)).ok_or_else(|| {
        Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "record range overflows",
        )
    })?;
    if end > meta.len() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!(
                "record range {offset}..{end} exceeds the source length {}",
                meta.len()
            ),
        ));
    }
    let mut file = File::open(source)
        .map_err(|e| Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(source, &e)))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(source, &e)))?;
    let mut buffer = vec![0u8; len_usize];
    file.read_exact(&mut buffer)
        .map_err(|e| Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(source, &e)))?;
    if let Some(expected) = expected_record_sha {
        let actual = hex_sha256(&buffer);
        if actual != expected {
            return Err(Failure::new(
                ATTACHMENT_STATUS_RECORD_MISMATCH,
                format!("record bytes hash to {actual}, expected {expected}"),
            ));
        }
    }
    if pointer.is_empty() {
        return Ok(Located::Raw(buffer));
    }
    let value: serde_json::Value = serde_json::from_slice(&buffer).map_err(|e| {
        Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!("record is not valid JSON: {e}"),
        )
    })?;
    let target = resolve_pointer(&value, pointer)?;
    match target {
        serde_json::Value::String(text) => Ok(Located::Text(text.clone())),
        other => Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!("pointer resolves to {}, not a string", json_kind(other)),
        )),
    }
}

fn locate_sqlite(
    source: &Path,
    table: &str,
    id: &str,
    column: &str,
    pointer: &str,
) -> Result<Located, Failure> {
    let meta = fs::metadata(source).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Failure::new(
            ATTACHMENT_STATUS_SOURCE_MISSING,
            format!("source does not exist: {}", source.display()),
        ),
        _ => Failure::new(ATTACHMENT_STATUS_READ_FAILED, io_message(source, &e)),
    })?;
    if !meta.is_file() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_READ_FAILED,
            format!("source is not a regular file: {}", source.display()),
        ));
    }
    validate_ident(table)?;
    validate_ident(column)?;
    let connection = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| {
            Failure::new(
                ATTACHMENT_STATUS_READ_FAILED,
                format!("sqlite open (read-only) failed: {e}"),
            )
        })?;
    let _ = connection.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));
    let sql = format!(
        "SELECT \"{column}\" FROM \"{table}\" WHERE \"id\" = ?1 LIMIT 1"
    );
    let row: Option<SqlValue> = connection
        .query_row(&sql, [id], |row| row.get(0))
        .optional()
        .map_err(|e| classify_sqlite_error(&e))?;
    let value = row.ok_or_else(|| {
        Failure::new(
            ATTACHMENT_STATUS_MISSING,
            format!("sqlite row not found: table={table} id={id}"),
        )
    })?;
    match value {
        SqlValue::Text(text) => {
            if pointer.is_empty() {
                Ok(Located::Text(text))
            } else {
                pointer_text(&text, pointer)
            }
        }
        SqlValue::Blob(bytes) => {
            if pointer.is_empty() {
                Ok(Located::Raw(bytes))
            } else {
                let text = String::from_utf8(bytes).map_err(|_| {
                    Failure::new(
                        ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                        "blob column is not UTF-8 JSON; use an empty pointer to archive the blob",
                    )
                })?;
                pointer_text(&text, pointer)
            }
        }
        SqlValue::Null => Err(Failure::new(
            ATTACHMENT_STATUS_MISSING,
            format!("sqlite column is NULL: table={table} id={id} column={column}"),
        )),
        other => Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!(
                "sqlite column is {}, not text or blob",
                sql_value_kind(&other)
            ),
        )),
    }
}

fn pointer_text(text: &str, pointer: &str) -> Result<Located, Failure> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!("column value is not valid JSON: {e}"),
        )
    })?;
    let target = resolve_pointer(&value, pointer)?;
    match target {
        serde_json::Value::String(text) => Ok(Located::Text(text.clone())),
        other => Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!("pointer resolves to {}, not a string", json_kind(other)),
        )),
    }
}

fn resolve_pointer<'a>(
    value: &'a serde_json::Value,
    pointer: &str,
) -> Result<&'a serde_json::Value, Failure> {
    if pointer.is_empty() {
        return Ok(value);
    }
    if !pointer.starts_with('/') {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "JSON pointer must start with '/'",
        ));
    }
    let mut current = value;
    for raw in pointer.split('/').skip(1) {
        let token = raw.replace("~1", "/").replace("~0", "~");
        current = match current {
            serde_json::Value::Object(map) => map.get(&token).ok_or_else(|| {
                Failure::new(
                    ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                    format!("pointer segment {token:?} not found"),
                )
            })?,
            serde_json::Value::Array(items) => {
                if token == "-" {
                    return Err(Failure::new(
                        ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                        "pointer array token '-' is not a location",
                    ));
                }
                let index = token.parse::<usize>().map_err(|_| {
                    Failure::new(
                        ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                        format!("pointer array index {token:?} is not numeric"),
                    )
                })?;
                items.get(index).ok_or_else(|| {
                    Failure::new(
                        ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                        format!("pointer array index {index} is out of range"),
                    )
                })?
            }
            _ => {
                return Err(Failure::new(
                    ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
                    format!("pointer segment {token:?} traverses a scalar"),
                ))
            }
        };
    }
    Ok(current)
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn sql_value_kind(value: &SqlValue) -> &'static str {
    match value {
        SqlValue::Null => "null",
        SqlValue::Integer(_) => "an integer",
        SqlValue::Real(_) => "a real",
        SqlValue::Text(_) => "text",
        SqlValue::Blob(_) => "a blob",
    }
}

fn validate_ident(value: &str) -> Result<(), Failure> {
    let mut chars = value.chars();
    let valid_start = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if !valid_start || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!("unsafe SQL identifier {value:?} (expected [A-Za-z_][A-Za-z0-9_]*)"),
        ));
    }
    Ok(())
}

fn classify_sqlite_error(error: &rusqlite::Error) -> Failure {
    let message = error.to_string();
    if message.contains("no such table") || message.contains("no such column") {
        return Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            format!("sqlite schema does not contain the reference: {message}"),
        );
    }
    if message.contains("locked") || message.contains("busy") {
        return Failure::new(
            ATTACHMENT_STATUS_READ_FAILED,
            format!("sqlite_locked: {message}"),
        );
    }
    Failure::new(
        ATTACHMENT_STATUS_READ_FAILED,
        format!("sqlite read failed: {message}"),
    )
}

// ---------------------------------------------------------------------------
// Path resolution (normalization + root boundary)
// ---------------------------------------------------------------------------

fn normalize_roots(raw: &[String]) -> Result<Vec<PathBuf>, ArchiveError> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for value in raw {
        let path = PathBuf::from(value);
        if value.contains('\0') {
            return Err(ArchiveError::Corrupt(
                "authorized root contains a NUL byte".to_string(),
            ));
        }
        if !path.is_absolute() {
            return Err(ArchiveError::Corrupt(format!(
                "authorized root {value:?} must be an absolute path"
            )));
        }
        let normalized = lexical_absolute(&path).map_err(|_| {
            ArchiveError::Corrupt(format!(
                "authorized root {value:?} escapes its own drive root"
            ))
        })?;
        if !roots.contains(&normalized) {
            roots.push(normalized);
        }
    }
    Ok(roots)
}

fn normalize_tags(raw: &[String]) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in raw {
        if tag.is_empty() {
            continue;
        }
        if !tags.iter().any(|existing| existing == tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

/// Resolve a reference value against `base` while enforcing the root boundary.
fn resolve_path_with_base(
    value: &str,
    base: &Path,
    roots: &[PathBuf],
) -> Result<PathBuf, Failure> {
    if value.is_empty() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "empty path reference",
        ));
    }
    if value.contains('\0') {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "path reference contains a NUL byte",
        ));
    }
    if classify_uri(value).is_some() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "URI is not a filesystem path",
        ));
    }
    let raw = Path::new(value);
    let normalized = if raw.has_root() {
        if !raw.is_absolute() {
            return Err(Failure::new(
                ATTACHMENT_STATUS_OUT_OF_BOUNDS,
                format!("root-relative path without a drive: {value}"),
            ));
        }
        let normalized = lexical_absolute(raw)?;
        if !roots.iter().any(|root| is_under(&normalized, root)) {
            return Err(Failure::new(
                ATTACHMENT_STATUS_OUT_OF_BOUNDS,
                format!("path is outside every authorized root: {value}"),
            ));
        }
        normalized
    } else {
        let relative = lexical_relative(raw)?;
        let joined = lexical_absolute(&base.join(relative))?;
        if !roots.iter().any(|root| is_under(&joined, root)) {
            return Err(Failure::new(
                ATTACHMENT_STATUS_OUT_OF_BOUNDS,
                format!("path is outside every authorized root: {value}"),
            ));
        }
        joined
    };

    match fs::metadata(&normalized) {
        Ok(meta) => {
            if meta.is_dir() {
                return Err(Failure::new(
                    ATTACHMENT_STATUS_READ_FAILED,
                    format!("not a regular file: {}", normalized.display()),
                ));
            }
            // Symlinks / junctions must not carry the read outside the roots.
            let canonical = fs::canonicalize(&normalized).map_err(|e| {
                Failure::new(
                    ATTACHMENT_STATUS_READ_FAILED,
                    io_message(&normalized, &e),
                )
            })?;
            let canonical_ok = roots.iter().any(|root| {
                let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                is_under(&canonical, &canonical_root)
            });
            if !canonical_ok {
                return Err(Failure::new(
                    ATTACHMENT_STATUS_OUT_OF_BOUNDS,
                    format!(
                        "path resolves outside every authorized root (symlink/junction): {}",
                        normalized.display()
                    ),
                ));
            }
            Ok(normalized)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Failure::new(
            ATTACHMENT_STATUS_MISSING,
            format!("file does not exist: {}", normalized.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(Failure::new(
            ATTACHMENT_STATUS_READ_FAILED,
            format!("permission denied: {}", normalized.display()),
        )),
        Err(e) => Err(Failure::new(
            ATTACHMENT_STATUS_READ_FAILED,
            io_message(&normalized, &e),
        )),
    }
}

fn lexical_relative(path: &Path) -> Result<PathBuf, Failure> {
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                out.push(part);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err(Failure::new(
                        ATTACHMENT_STATUS_PATH_TRAVERSAL,
                        format!("relative reference escapes its base: {}", path.display()),
                    ));
                }
                out.pop();
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Failure::new(
                    ATTACHMENT_STATUS_OUT_OF_BOUNDS,
                    format!("rooted component inside a relative path: {}", path.display()),
                ))
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "relative reference resolves to the base directory",
        ));
    }
    Ok(out)
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, Failure> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return Err(Failure::new(
                        ATTACHMENT_STATUS_OUT_OF_BOUNDS,
                        format!("path escapes its drive root: {}", path.display()),
                    ));
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if !out.is_absolute() {
        return Err(Failure::new(
            ATTACHMENT_STATUS_OUT_OF_BOUNDS,
            format!("path is not absolute after normalization: {}", path.display()),
        ));
    }
    Ok(out)
}

/// Comparable form of a path: verbatim prefix removed, forward slashes,
/// lowercased on Windows (case-insensitive filesystem), no trailing separator.
fn path_key(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('/', "\\");
    let stripped = if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw
    };
    let trimmed = stripped.trim_end_matches('\\');
    let value = if trimmed.is_empty() {
        stripped.as_str()
    } else {
        trimmed
    };
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.to_string()
    }
}

fn is_under(child: &Path, root: &Path) -> bool {
    let child = path_key(child);
    let root = path_key(root);
    if child == root {
        return true;
    }
    let prefix = if root.ends_with('\\') {
        root
    } else {
        format!("{root}\\")
    };
    child.starts_with(&prefix)
}

// ---------------------------------------------------------------------------
// URI / data URI / media type
// ---------------------------------------------------------------------------

fn classify_uri(value: &str) -> Option<UriKind> {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("data:") {
        return Some(UriKind::Data);
    }
    if lower.starts_with("file://") {
        return Some(UriKind::File);
    }
    if let Some(index) = value.find("://") {
        if is_scheme(&value[..index]) {
            return Some(UriKind::External);
        }
    }
    None
}

fn is_scheme(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

fn file_url_to_path(value: &str) -> String {
    let rest = &value["file://".len()..];
    // `file:///C:/x` -> `C:/x`; `file://server/share` stays a relative candidate
    // and is therefore rejected by the root boundary instead of guessed as UNC.
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    rest.to_string()
}

fn parse_data_uri(value: &str) -> Result<(String, Vec<u8>), Failure> {
    let rest = &value["data:".len()..];
    let (meta, data) = rest.split_once(',').ok_or_else(|| {
        Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "data URI has no comma separator",
        )
    })?;
    if !meta.to_ascii_lowercase().contains(";base64") {
        return Err(Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "only base64 data URIs are archived (percent-encoded payloads are not decoded)",
        ));
    }
    let media_type = meta
        .split(';')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = base64_decode(data).ok_or_else(|| {
        Failure::new(
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE,
            "data URI payload is not standard base64",
        )
    })?;
    Ok((media_type, bytes))
}

fn base64_decode(raw: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    let mut padding = 0usize;
    for &byte in raw.as_bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return None;
        }
        let sextet = value(byte)?;
        accumulator = (accumulator << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1u32 << bits) - 1;
        }
    }
    if padding > 2 || bits >= 6 {
        return None;
    }
    Some(out)
}

/// Best-effort media type from magic bytes; UTF-8 text falls back to
/// `text/plain`. The detected value is a capture fact, not a parser judgement.
pub fn detect_media_type(bytes: &[u8]) -> String {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if bytes.starts_with(b"\x1f\x8b") {
        "application/gzip"
    } else if std::str::from_utf8(bytes).is_ok() {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
    .to_string()
}

fn read_head(path: &Path, max: u64) -> Result<Vec<u8>, ArchiveError> {
    let mut file = File::open(path).map_err(|e| AppError::io(path, e))?;
    let mut buffer = vec![0u8; max as usize];
    let mut filled = 0usize;
    while filled < buffer.len() {
        let read = file
            .read(&mut buffer[filled..])
            .map_err(|e| AppError::io(path, e))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn source_fingerprint(
    path: &Path,
) -> Result<(SourceFingerprint, String), ArchiveError> {
    match fs::metadata(path) {
        Ok(_) => Ok((crate::capture::fingerprint(path)?, "present".to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((
            SourceFingerprint {
                mtime_ms: 0,
                len: 0,
                head_sha256: String::new(),
                tail_sha256: String::new(),
            },
            "missing".to_string(),
        )),
        Err(e) => Err(AppError::io(path, e).into()),
    }
}

fn io_message(path: &Path, error: &std::io::Error) -> String {
    format!("{}: {error}", path.display())
}

fn describe_ref(content_ref: &ContentRef) -> String {
    match content_ref {
        ContentRef::Jsonl {
            offset,
            len,
            pointer,
        } => format!("jsonl@{offset}+{len}#{pointer}"),
        ContentRef::Sqlite {
            table,
            id,
            column,
            pointer,
        } => format!("sqlite:{table}[id={id}].{column}#{pointer}"),
        ContentRef::File { rel_path, pointer } => format!("file:{rel_path}#{pointer}"),
        ContentRef::Sidecar { rel_path } => format!("sidecar:{rel_path}"),
    }
}
