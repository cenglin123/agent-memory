//! `memory_read`: fail-closed readback with `segments[]`, context windows,
//! per-segment fingerprints and byte budgets.
//!
//! Text sources are read through seeks and bounded range reads; OpenCode refs
//! are resolved to exactly one part row / part file. Nothing here reads a whole
//! file into memory.

use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{
    codes, RecallError, SideEffect, WarningEntry, ALL_SCHEMA_GENS, READ_CONTEXT_TURN,
    READ_REQUEST_KEY_CONTEXT, REF_KEYS, SCHEMA_ARCHIVE_RECORD_V1, SCHEMA_CLAUDE_JSONL_V1,
    SCHEMA_CODEX_ROLLOUT_V1, SCHEMA_DSH_JSONL, SCHEMA_JSON_LEGACY_V1, SCHEMA_MD_V1,
    SCHEMA_SQLITE_V1, SCHEMA_SQLITE_V2,
};
use crate::recall::domain::{classify_access, AccessDecision};
use crate::recall::hash::{file_sha256_streaming, root_sha256, sha256_hex, TextRange};
use crate::recall::port::model::PortError;
use crate::recall::port::{classify_part, read_line_bounded, resolve, BoundedLine, PartOutcome};
use crate::recall::providers::archive::{self as archive_provider, Unit};
use crate::recall::providers::claude_code::{self, ClaudeRecordOutcome};
use crate::recall::providers::codex::{
    codex_storage_dir, project_record, RecordOutcome, CODEX_SCAN_LIMIT_BYTES,
};
use crate::recall::providers::dsh::{self, DshEventOutcome};
use crate::recall::providers::hermes;
use crate::recall::refs::{
    is_valid_json_pointer, parse_content_pointer, Fingerprint, HitRef, Locator,
};
use crate::recall::snapshot::Window;
use crate::recall::text::utf8_floor;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const READ_KEYS: [&str; 7] = [
    "ref",
    "context_before",
    "context_after",
    "max_bytes",
    "domain",
    "context",
    "session_id",
];
const FINGERPRINT_KEYS: [&str; 3] = ["range_sha256", "file_sha256", "row_sha256"];
const TEXT_RANGE_KEYS: [&str; 4] = ["line_start", "line_end", "byte_start", "byte_end"];
const MAX_REF_BYTES: u64 = 64 * 1024 * 1024;
/// `session_meta` (the Codex subagent marker) is the first record of a rollout;
/// scanning a bounded prefix is enough to classify the source without reading the
/// whole session for every turn read.
const SUBAGENT_SCAN_RECORDS: usize = 8;

/// How the caller wants the referenced record contextualised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMode {
    /// P0–P3 window: `context_before` / `context_after` records.
    Window,
    /// P4: the complete turn that contains the referenced record.
    Turn,
}

#[derive(Debug, Clone)]
pub struct ReadRequest {
    pub reference: HitRef,
    pub context_before: usize,
    pub context_after: usize,
    pub max_bytes: usize,
    /// Request-layer access domain; the ref itself never carries one.
    pub domain: Option<String>,
    pub context: ContextMode,
    /// Optional caller session id. Device reads use it only for the local
    /// reuse counter (dedup); it never changes which bytes are returned.
    pub session_id: Option<String>,
}

impl ReadRequest {
    pub fn parse(value: &Value) -> Result<ReadRequest, RecallError> {
        let map = value
            .as_object()
            .ok_or_else(|| invalid_type("request", "object"))?;
        reject_unknown(map, &READ_KEYS, "")?;
        let reference = parse_ref(
            map.get("ref")
                .ok_or_else(|| RecallError::invalid("ref", "missing_key", "ref is required"))?,
        )?;
        let context_before =
            optional_integer(map.get("context_before"), "context_before", 0, 1_000, 0)?;
        let context_after =
            optional_integer(map.get("context_after"), "context_after", 0, 1_000, 0)?;
        let max_bytes = optional_integer(map.get("max_bytes"), "max_bytes", 1, 1_048_576, 8_192)?;
        let domain = match map.get("domain") {
            None => None,
            Some(value) => Some(crate::recall::domain::parse_domain_value(value, "domain")?),
        };
        let context = match map.get(READ_REQUEST_KEY_CONTEXT) {
            None => ContextMode::Window,
            Some(Value::String(value)) if value == READ_CONTEXT_TURN => ContextMode::Turn,
            Some(_) => {
                return Err(RecallError::invalid(
                    READ_REQUEST_KEY_CONTEXT,
                    "unsupported_context",
                    "context must be \"turn\" (the only non-default mode)",
                ))
            }
        };
        if context == ContextMode::Turn && (context_before > 0 || context_after > 0) {
            // Two context languages in one request would make the assembled
            // segments ambiguous; the caller must pick one.
            return Err(RecallError::invalid(
                READ_REQUEST_KEY_CONTEXT,
                "conflicting_context",
                "context \"turn\" must not be combined with context_before/context_after",
            ));
        }
        let session_id = match map.get("session_id") {
            None => None,
            Some(Value::String(value)) if !value.is_empty() && value.len() <= 200 => {
                Some(value.clone())
            }
            Some(_) => {
                return Err(RecallError::invalid(
                    "session_id",
                    "invalid_session_id",
                    "session_id must be a non-empty string of at most 200 bytes",
                ))
            }
        };
        Ok(ReadRequest {
            reference,
            context_before,
            context_after,
            max_bytes,
            domain,
            context,
            session_id,
        })
    }
}

impl ReadRequest {
    pub fn effective_domain(&self, config: &RecallConfig) -> String {
        config.effective_domain(self.domain.as_deref())
    }

    pub fn domain_is_explicit(&self, config: &RecallConfig) -> bool {
        config.domain_is_explicit(self.domain.as_deref())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub locator: Locator,
    pub text_range: TextRange,
    pub range_sha256: String,
    pub text: String,
    /// P4: present only for `context: "turn"` assembly. Each segment carries its
    /// own `archive-record-v1` ref so it can be read back on its own, instead of
    /// being an unattributable slice of a turn.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<HitRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    pub segments: Vec<Segment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_range: Option<TextRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_sha256: Option<String>,
    pub truncated: bool,
    pub warnings: Vec<WarningEntry>,
    pub side_effects: Vec<SideEffect>,
    #[serde(skip)]
    pub exit_code: i32,
}

pub struct ReadOutcome {
    segments: Vec<Segment>,
    warnings: Vec<WarningEntry>,
    truncated: bool,
}

impl ReadResponse {
    /// Request-level failure in the frozen `memory_read` shape.
    pub fn request_failure(error: &RecallError) -> Self {
        ReadResponse::failure(error, Vec::new())
    }

    fn failure(error: &RecallError, side_effects: Vec<SideEffect>) -> Self {
        ReadResponse {
            ok: false,
            code: Some(error.code),
            segments: Vec::new(),
            text: None,
            text_range: None,
            range_sha256: None,
            truncated: false,
            warnings: Vec::new(),
            side_effects,
            exit_code: error.exit_code(),
        }
    }

    fn success(outcome: ReadOutcome, side_effects: Vec<SideEffect>) -> Self {
        let single = outcome.segments.len() == 1;
        let (text, text_range, range_sha256) = if single {
            let segment = &outcome.segments[0];
            (
                Some(segment.text.clone()),
                Some(segment.text_range),
                Some(segment.range_sha256.clone()),
            )
        } else {
            (None, None, None)
        };
        ReadResponse {
            ok: true,
            code: None,
            segments: outcome.segments,
            text,
            text_range,
            range_sha256,
            truncated: outcome.truncated,
            warnings: outcome.warnings,
            side_effects,
            exit_code: 0,
        }
    }
}

pub fn memory_read(config: &RecallConfig, request: &ReadRequest) -> ReadResponse {
    memory_read_with(config, request, &ReadOptions::default())
}

/// Execution options shared with the CLI / stdio MCP wrapper.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOptions {
    pub strict_snapshot: bool,
}

pub fn memory_read_with(
    config: &RecallConfig,
    request: &ReadRequest,
    options: &ReadOptions,
) -> ReadResponse {
    let reference = &request.reference;
    let Some(source) = config.source(&reference.source_id) else {
        return ReadResponse::failure(
            &RecallError::new(
                codes::REF_SOURCE_MISMATCH,
                "ref source_id is not configured",
            ),
            Vec::new(),
        );
    };
    if !source.enabled {
        return ReadResponse::failure(
            &RecallError::new(
                codes::REF_SOURCE_MISMATCH,
                "ref source_id is disabled in the configuration",
            ),
            Vec::new(),
        );
    }
    if source.kind.as_str() != reference.source_kind {
        return ReadResponse::failure(
            &RecallError::new(
                codes::REF_SOURCE_MISMATCH,
                "ref source_kind does not match the configured source",
            ),
            Vec::new(),
        );
    }
    if root_sha256(&source.root) != reference.root_sha256 {
        return ReadResponse::failure(
            &RecallError::new(
                codes::REF_ROOT_MISMATCH,
                "ref root_sha256 does not match the configured root",
            ),
            Vec::new(),
        );
    }
    let schema_ok = match source.kind {
        SourceKind::Kb | SourceKind::Device => reference.schema_gen == SCHEMA_MD_V1,
        SourceKind::Docs => reference.schema_gen == crate::recall::contract::SCHEMA_DOCS_MD_V1,
        SourceKind::Opencode => matches!(
            reference.schema_gen.as_str(),
            SCHEMA_JSON_LEGACY_V1 | SCHEMA_SQLITE_V1 | SCHEMA_SQLITE_V2
        ),
        SourceKind::Codex => reference.schema_gen == SCHEMA_CODEX_ROLLOUT_V1,
        SourceKind::ClaudeCode => reference.schema_gen == SCHEMA_CLAUDE_JSONL_V1,
        SourceKind::Dsh => reference.schema_gen == SCHEMA_DSH_JSONL,
        SourceKind::Hermes => {
            reference.schema_gen == crate::recall::contract::SCHEMA_HERMES_SQLITE_V1
        }
        SourceKind::Archive => reference.schema_gen == SCHEMA_ARCHIVE_RECORD_V1,
    };
    if !schema_ok {
        return ReadResponse::failure(
            &RecallError::new(
                codes::REF_SOURCE_MISMATCH,
                "ref schema_gen is not valid for this source kind",
            ),
            Vec::new(),
        );
    }
    if request.context == ContextMode::Turn && source.kind != SourceKind::Archive {
        // Only Archive records have a proven intra-source order; a live source
        // would have to guess which records form a turn.
        return ReadResponse::failure(
            &RecallError::invalid(
                "context",
                "unsupported_for_source",
                "context \"turn\" is only supported for archive sources",
            ),
            Vec::new(),
        );
    }

    // Stage-2 S5 archive fallback: the ref's locator marks it as built from
    // committed evidence of a session that is gone from the live source. The
    // bytes come from the archive configured alongside the source; the live
    // root is never opened, and nothing is written (MCP stays read-only).
    if reference.locator.archive_fallback.is_some() {
        if options.strict_snapshot {
            return ReadResponse::failure(
                &RecallError::new(
                    codes::DB_UNAVAILABLE,
                    "--strict-snapshot is not supported for archive-fallback refs \
                     (fallback_unsupported_strict_snapshot)",
                ),
                Vec::new(),
            );
        }
        return match crate::recall::fulltext::fallback::read_fallback(config, source, request) {
            Ok(outcome) => ReadResponse::success(outcome, Vec::new()),
            Err(error) => ReadResponse::failure(&error, Vec::new()),
        };
    }

    if options.strict_snapshot && source.kind == SourceKind::Codex {
        // Copying a Codex home would clone `auth.json` / logs and can exceed the
        // snapshot ceilings; live rollout files are also append-only. Refuse
        // explicitly and copy nothing.
        return ReadResponse::failure(
            &RecallError::new(
                codes::DB_UNAVAILABLE,
                "--strict-snapshot is not supported for codex sources \
                 (strict_snapshot_unsupported_for_codex_home)",
            ),
            Vec::new(),
        );
    }
    if options.strict_snapshot && source.kind == SourceKind::ClaudeCode {
        // Claude Code transcripts are live append files under a projects tree
        // that also carries sidecar tool results: a copy could freeze a
        // half-written line. Refuse explicitly and copy nothing.
        return ReadResponse::failure(
            &RecallError::new(
                codes::DB_UNAVAILABLE,
                "--strict-snapshot is not supported for claude_code sources \
                 (strict_snapshot_unsupported_for_claude_code_root)",
            ),
            Vec::new(),
        );
    }
    if options.strict_snapshot && source.kind == SourceKind::Dsh {
        // DSH session logs are live append-only frame streams: a copy could
        // freeze a torn frame and its own migration could publish a newer
        // generation while the copy is taken. Refuse explicitly.
        return ReadResponse::failure(
            &RecallError::new(
                codes::DB_UNAVAILABLE,
                "--strict-snapshot is not supported for dsh sources \
                 (strict_snapshot_unsupported_for_dsh_root)",
            ),
            Vec::new(),
        );
    }
    if source.kind == SourceKind::Archive {
        // Archive objects are immutable and content-addressed: a read is
        // already a snapshot, and copying the whole object store per read is
        // not a bounded operation.
        if options.strict_snapshot {
            return ReadResponse::failure(
                &RecallError::new(
                    codes::DB_UNAVAILABLE,
                    "--strict-snapshot is not supported for archive sources",
                ),
                Vec::new(),
            );
        }
        let effective_domain = config.effective_domain(request.domain.as_deref());
        let explicit = config.domain_is_explicit(request.domain.as_deref());
        return match read_archive(source, request, &effective_domain, explicit) {
            Ok(outcome) => ReadResponse::success(outcome, Vec::new()),
            Err(error) => ReadResponse::failure(&error, Vec::new()),
        };
    }

    let snapshot = if options.strict_snapshot {
        match crate::recall::snapshot::StrictSnapshot::create(Path::new(&source.root)) {
            Ok(snapshot) => Some(snapshot),
            Err(reason) => {
                return ReadResponse::failure(
                    &RecallError::new(
                        codes::DB_UNAVAILABLE,
                        format!("--strict-snapshot could not copy the source root: {reason}"),
                    ),
                    Vec::new(),
                )
            }
        }
    } else {
        None
    };
    let read_root = snapshot
        .as_ref()
        .map(|snapshot| snapshot.path().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(&source.root));
    let window = if snapshot.is_none()
        && source.kind != SourceKind::Codex
        && source.kind != SourceKind::ClaudeCode
        && source.kind != SourceKind::Dsh
    {
        // Codex rollouts, Claude Code transcripts and DSH session logs are live
        // append files: an observed-root diff would report the agent's own
        // writes as a contract violation.
        Some(Window::open(&read_root))
    } else {
        None
    };
    let outcome = match source.kind {
        SourceKind::Kb | SourceKind::Device => read_text(request, &read_root),
        SourceKind::Docs => read_docs(source, request, &read_root),
        SourceKind::Opencode => read_opencode(source, request, &read_root),
        SourceKind::Codex => read_codex(source, request, &read_root),
        SourceKind::ClaudeCode => read_claude_code(source, request, &read_root),
        SourceKind::Dsh => read_dsh(source, request, &read_root),
        SourceKind::Hermes => read_hermes(source, request, &read_root),
        SourceKind::Archive => Err(RecallError::new(
            codes::REF_READ_FAILED,
            "archive readback must never take the snapshot path",
        )),
    };
    let (side_effects, violations) = match window {
        Some(window) => window.close(),
        None => (Vec::new(), Vec::new()),
    };
    if !violations.is_empty() {
        return ReadResponse::failure(
            &RecallError::new(
                codes::SIDE_EFFECT_VIOLATION,
                format!("observed root changed outside C-level runtime artifacts: {violations:?}"),
            ),
            side_effects,
        );
    }
    match outcome {
        Ok(outcome) => {
            let mut side_effects = side_effects;
            if source.kind == SourceKind::Device && !options.strict_snapshot {
                if let Some(effect) = record_touch(config, source, request) {
                    side_effects.push(effect);
                }
            }
            ReadResponse::success(outcome, side_effects)
        }
        Err(error) => ReadResponse::failure(&error, side_effects),
    }
}

/// Append one local device-read touch under the cache root.
///
/// This is a declared derived side effect, not a source write: the event file
/// is opened read-only above and is never modified. A failure to record the
/// touch never fails the read.
fn record_touch(
    config: &RecallConfig,
    source: &SourceConfig,
    request: &ReadRequest,
) -> Option<SideEffect> {
    let cache = config.cache_root.as_deref()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let mut entry = serde_json::json!({
        "rel_path": request.reference.rel_path,
        "ts": stamp,
    });
    if let Some(session) = request.session_id.as_deref() {
        entry["session_id"] = Value::String(session.to_string());
    }
    let name = format!("{}.jsonl", source.source_id);
    let line = format!("{entry}\n");
    crate::cache_store::append(Path::new(cache), &["feedback", &name], line.as_bytes()).ok()?;
    Some(SideEffect::window_change(format!(
        "{cache}/feedback/{name}"
    )))
}

/// Read one byte span out of an opened file and build its segment.
/// `start`/`end` are absolute file offsets; `line_start` is the 0-based line
/// of `start`, which the caller computes (a prefix for text sources, a prefix
/// scan for docs chunks).
#[allow(clippy::too_many_arguments)]
fn read_span_segment(
    file: &mut File,
    start: u64,
    end: u64,
    line_start: u64,
    file_len: u64,
    request: &ReadRequest,
    reference: &HitRef,
) -> Result<(Segment, bool), RecallError> {
    let ref_bytes = read_range(file, start, end)
        .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "range read failed"))?;
    if sha256_hex(&ref_bytes) != reference.fingerprint.range_sha256 {
        return Err(RecallError::new(
            codes::REF_STALE,
            "range_sha256 does not match the current bytes",
        ));
    }

    let before_start = if request.context_before > 0 {
        find_line_start(file, start, request.context_before + 1)
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "backward scan failed"))?
    } else {
        start
    };
    let after_end = if request.context_after > 0 {
        find_forward_end(file, end, request.context_after, file_len)
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "forward scan failed"))?
    } else {
        end
    };

    let target_len = end.saturating_sub(start);
    let budget = request.max_bytes as u64;
    let mut seg_line_start = line_start;
    let (mut bytes, seg_start, truncated) = if target_len > budget {
        // The verified target has priority over both context sides. Only when
        // the target alone exceeds the budget may the target itself be cut.
        let take = (budget + 3).min(target_len);
        let raw = read_range(file, start, start + take)
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "range read failed"))?;
        let cut = utf8_floor_bytes(&raw, request.max_bytes);
        (raw[..cut].to_vec(), start, true)
    } else {
        let remaining = budget - target_len;
        let before_available = start - before_start;
        let after_available = after_end - end;
        let mut before_take = before_available.min(remaining / 2);
        let mut after_take = after_available.min(remaining - remaining / 2);

        // Give unused capacity to the side that can still consume it. When both
        // sides exceed their half, the original even split remains unchanged.
        let mut spare = remaining - before_take - after_take;
        let extra_before = (before_available - before_take).min(spare);
        before_take += extra_before;
        spare -= extra_before;
        let extra_after = (after_available - after_take).min(spare);
        after_take += extra_after;

        let desired_start = start - before_take;
        let desired_end = end + after_take;
        let mut before = read_range(file, desired_start, start)
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "range read failed"))?;
        let mut after = read_range(file, end, desired_end)
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "range read failed"))?;

        // Byte budgets can begin/end in the middle of a codepoint. Trim only
        // the exposed context edge; the complete target remains untouched.
        let prefix = before
            .iter()
            .take(4)
            .take_while(|byte| **byte & 0b1100_0000 == 0b1000_0000)
            .count();
        before.drain(..prefix);
        if let Err(error) = std::str::from_utf8(&after) {
            if error.error_len().is_none() {
                if after_take >= after_available {
                    // The whole requested context was captured, so a dangling
                    // sequence is the file's own invalid trailing UTF-8, not a
                    // budget cut. Stay fail-closed like the pre-budget code.
                    return Err(RecallError::new(
                        codes::REF_READ_FAILED,
                        "file is not UTF-8",
                    ));
                }
                after.truncate(error.valid_up_to());
            }
        }

        let actual_start = desired_start + prefix as u64;
        let before_newlines = before.iter().filter(|byte| **byte == b'\n').count() as u64;
        seg_line_start = line_start.saturating_sub(before_newlines);
        let mut assembled = Vec::with_capacity(before.len() + ref_bytes.len() + after.len());
        assembled.extend_from_slice(&before);
        assembled.extend_from_slice(&ref_bytes);
        assembled.extend_from_slice(&after);
        let clipped = before_take < before_available || after_take < after_available;
        (assembled, actual_start, clipped)
    };
    if bytes.is_empty() {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "readback produced an empty segment",
        ));
    }
    // Re-check that truncation never splits a multi-byte character.
    if truncated {
        while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
            bytes.pop();
        }
    }
    // The pop loop can empty a target that consisted only of continuation
    // bytes (a ref range that starts inside a codepoint). The pre-loop check
    // above cannot see that, so guard again here.
    if bytes.is_empty() {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "readback produced an empty segment",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "file is not UTF-8"))?;
    let newlines = text.bytes().filter(|byte| *byte == b'\n').count() as u64;
    let tail = if text.ends_with('\n') { 0 } else { 1 };
    let text_range = TextRange::new(
        seg_start,
        seg_start + text.len() as u64,
        seg_line_start,
        seg_line_start + newlines + tail,
    );
    let segment = Segment {
        locator: reference.locator.clone(),
        text_range,
        range_sha256: sha256_hex(text.as_bytes()),
        text,
        reference: None,
    };

    Ok((segment, truncated))
}

fn read_text(request: &ReadRequest, read_root: &Path) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    if reference.schema_gen != SCHEMA_MD_V1 {
        return Err(RecallError::new(
            codes::REF_SOURCE_MISMATCH,
            "text sources use md-v1",
        ));
    }
    let (path, _canonical_root, mut file, file_len) =
        open_text_at_root(read_root, &reference.rel_path)?;
    let range = reference.text_range;
    if range.byte_start > range.byte_end || range.byte_end > file_len {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "text_range is outside the file",
        ));
    }

    let (segment, truncated) = read_span_segment(
        &mut file,
        range.byte_start,
        range.byte_end,
        range.line_start,
        file_len,
        request,
        reference,
    )?;
    let mut warnings = Vec::new();
    if let Some(expected) = &reference.fingerprint.file_sha256 {
        let observed = file_sha256_streaming(&path)
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "file hash failed"))?;
        if &observed != expected {
            warnings.push(WarningEntry::file_stale(
                Some(reference.source_id.clone()),
                Some(serde_json::json!({
                    "expected": expected,
                    "observed": observed,
                })),
            ));
        }
    }

    Ok(ReadOutcome {
        segments: vec![segment],
        warnings,
        truncated,
    })
}

/// `docs` refs point at a heading chunk: the locator carries the chunk's start
/// byte (`/chunk/<n>`) and `range_len` its length, so a file with many chunks
/// stays addressable without widening the index schema.
fn read_docs(
    source: &SourceConfig,
    request: &ReadRequest,
    read_root: &Path,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    if reference.schema_gen != crate::recall::contract::SCHEMA_DOCS_MD_V1 {
        return Err(RecallError::new(
            codes::REF_SOURCE_MISMATCH,
            "docs sources use docs-md-v1",
        ));
    }
    if !crate::recall::docs::admit_rel_path(&reference.rel_path, &source.include, &source.exclude) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "not-admitted: docs rel_path fails the source file-set rules",
        ));
    }
    let (path, canonical_root, mut file, file_len) =
        open_text_at_root(read_root, &reference.rel_path)?;
    // The literal rel_path passed the rules, but on case-insensitive or short
    // name bearing volumes (NTFS 8.3 aliases) the file the path resolves to can
    // still be an excluded carrier. Re-run the same rules against the canonical
    // root-relative name so an alias cannot borrow an admitted spelling.
    let resolved_rel = rel_under_root(&canonical_root, &path);
    match resolved_rel {
        Some(rel)
            if crate::recall::docs::admit_rel_path(&rel, &source.include, &source.exclude) => {}
        _ => {
            return Err(RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "not-admitted: resolved docs path fails the source file-set rules",
            ))
        }
    }
    let start = docs_chunk_start(reference)?;
    let range = reference.text_range;
    let len = range.byte_end.saturating_sub(range.byte_start);
    let end = start.saturating_add(len).min(file_len);
    if start > file_len || start > end {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "chunk is outside the file",
        ));
    }
    let line_start = count_newlines_before(&mut file, start)?;
    let (segment, truncated) = read_span_segment(
        &mut file, start, end, line_start, file_len, request, reference,
    )?;

    let mut warnings = Vec::new();
    if let Some(expected) = &reference.fingerprint.file_sha256 {
        let observed = file_sha256_streaming(&path)
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "file hash failed"))?;
        if &observed != expected {
            warnings.push(WarningEntry::file_stale(
                Some(reference.source_id.clone()),
                Some(serde_json::json!({
                    "expected": expected,
                    "observed": observed,
                })),
            ));
        }
    }

    Ok(ReadOutcome {
        segments: vec![segment],
        warnings,
        truncated,
    })
}

fn docs_chunk_start(reference: &HitRef) -> Result<u64, RecallError> {
    let Some(pointer) = reference.locator.pointer.as_deref() else {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "docs ref must carry a chunk pointer",
        ));
    };
    let Some(rest) = pointer.strip_prefix("/chunk/") else {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "docs locator is not a chunk pointer",
        ));
    };
    rest.parse::<u64>().map_err(|_| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "docs chunk pointer is malformed",
        )
    })
}

fn count_newlines_before(file: &mut File, end: u64) -> Result<u64, RecallError> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "seek failed"))?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut consumed = 0u64;
    let mut count = 0u64;
    while consumed < end {
        let want = ((end - consumed) as usize).min(buffer.len());
        let read = file
            .read(&mut buffer[..want])
            .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "prefix read failed"))?;
        if read == 0 {
            break;
        }
        count += buffer[..read].iter().filter(|byte| **byte == b'\n').count() as u64;
        consumed += read as u64;
    }
    Ok(count)
}

fn read_opencode(
    source: &SourceConfig,
    request: &ReadRequest,
    read_root: &Path,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    // Rel paths / containment follow the tree actually read (configured root or
    // its strict-snapshot copy); `root_sha256` still comes from the config.
    let read_root_str = read_root.to_string_lossy().to_string();
    let mapped_db = remap_configured(read_root, &source.root, source.db.as_deref());
    let mapped_storage = remap_configured(read_root, &source.root, source.storage.as_deref());
    let mut resolved = resolve(
        read_root,
        &read_root_str,
        mapped_db.as_deref(),
        mapped_storage.as_deref(),
    )
    .map_err(map_read_error)?;
    if resolved.generation != reference.schema_gen {
        return Err(RecallError::new(
            codes::REF_SOURCE_MISMATCH,
            "ref schema_gen does not match the resolved generation",
        ));
    }

    read_parts_after_resolve(&mut resolved.reader, request)
}

/// Everything [`read_opencode`] does after the carrier is resolved: load the
/// referenced part, verify rel_path + projection range + row fingerprint, and
/// assemble the context window. Shared with the stage-2 S5 archive-fallback
/// readback, which resolves the carrier from evidence instead of the live root.
pub(crate) fn read_parts_after_resolve(
    reader: &mut crate::recall::port::reader::Reader,
    request: &ReadRequest,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let part = reader
        .load_part(&reference.locator)
        .map_err(map_read_error)?;
    // §3.2: `rel_path` names the carrier the locator resolves to. A ref whose
    // carrier does not match the resolved part (wrong db path / wrong part
    // file) is rejected instead of silently reading the locator's part.
    if part.rel_path != reference.rel_path {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            format!(
                "ref.rel_path {} does not match the resolved carrier {}",
                reference.rel_path, part.rel_path
            ),
        ));
    }
    // A side-table ref pins the exact row bytes. Check before projection or
    // range validation so shortening/corrupting a part is also REF_STALE.
    let strict_row = part.schema_gen == SCHEMA_SQLITE_V2 && part.locator.part_id.is_some();
    if strict_row && reference.fingerprint.row_sha256.as_deref() != Some(part.row_sha256.as_str()) {
        return Err(RecallError::new(
            codes::REF_STALE,
            "row_sha256 does not match the current session_part data",
        ));
    }
    let projection = match classify_part(&part.data) {
        PartOutcome::Text { projection, .. } if !projection.is_empty() => projection,
        PartOutcome::Text { .. } => {
            return Err(RecallError::new(
                codes::REF_OUT_OF_RANGE,
                "ref points at an empty part projection",
            ))
        }
        PartOutcome::Injected | PartOutcome::Ignored { .. } => {
            return Err(RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "ref points at a non-text part",
            ))
        }
        PartOutcome::Unresolved { reason } => {
            return Err(RecallError::new(
                codes::REF_READ_FAILED,
                format!("part projection failed: {reason}"),
            ))
        }
    };
    let projection_bytes = projection.as_bytes();
    let range = reference.text_range;
    if range.byte_start > range.byte_end || range.byte_end > projection_bytes.len() as u64 {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "text_range is outside the part projection",
        ));
    }
    let start = range.byte_start as usize;
    let end = range.byte_end as usize;
    if sha256_hex(&projection_bytes[start..end]) != reference.fingerprint.range_sha256 {
        return Err(RecallError::new(
            codes::REF_STALE,
            "range_sha256 does not match the current projection",
        ));
    }

    let mut warnings = Vec::new();
    if let Some(expected) = &reference.fingerprint.row_sha256 {
        if &part.row_sha256 != expected {
            warnings.push(WarningEntry::file_stale(
                Some(reference.source_id.clone()),
                Some(serde_json::json!({
                    "expected": expected,
                    "observed": part.row_sha256,
                })),
            ));
        }
    }

    let windows: Vec<(Locator, String)> =
        if request.context_before == 0 && request.context_after == 0 {
            vec![(part.locator.clone(), projection.clone())]
        } else {
            let mut health = crate::recall::port::Health::default();
            let ordered = reader
                .parts_for_session(part.session_id(), 64 * 1024 * 1024, &mut health)
                .map_err(map_read_error)?;
            let mut eligible: Vec<(Locator, String)> = Vec::new();
            for candidate in ordered {
                if let PartOutcome::Text { projection, .. } = classify_part(&candidate.data) {
                    if !projection.is_empty() {
                        eligible.push((candidate.locator.clone(), projection));
                    }
                }
            }
            let position = eligible
                .iter()
                .position(|(locator, _)| locator.same_coordinates(&reference.locator))
                .ok_or_else(|| {
                    RecallError::new(codes::REF_LOCATOR_MISSING, "hit part not in its session")
                })?;
            let start = position.saturating_sub(request.context_before);
            let end = (position + 1 + request.context_after).min(eligible.len());
            eligible[start..end].to_vec()
        };

    let mut segments = Vec::new();
    let mut remaining = request.max_bytes;
    let mut truncated = false;
    for (locator, text) in windows.iter() {
        if remaining == 0 {
            truncated = true;
            break;
        }
        let bytes = text.as_bytes();
        if bytes.len() <= remaining {
            segments.push(Segment {
                locator: locator.clone(),
                text_range: TextRange::from_span(bytes, 0, bytes.len()),
                range_sha256: sha256_hex(bytes),
                text: text.clone(),
                reference: None,
            });
            remaining -= bytes.len();
        } else {
            let cut = utf8_floor(text, remaining);
            if cut == 0 {
                truncated = true;
                break;
            }
            let prefix = &text[..cut];
            segments.push(Segment {
                locator: locator.clone(),
                text_range: TextRange::from_span(prefix.as_bytes(), 0, prefix.len()),
                range_sha256: sha256_hex(prefix.as_bytes()),
                text: prefix.to_string(),
                reference: None,
            });
            truncated = true;
            break;
        }
    }
    if segments.is_empty() {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "readback produced no segment",
        ));
    }

    Ok(ReadOutcome {
        segments,
        warnings,
        truncated,
    })
}

fn read_codex(
    source: &SourceConfig,
    request: &ReadRequest,
    read_root: &Path,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let locator = &reference.locator;
    let session_id = locator.session_id.as_deref().unwrap_or("");
    if session_id.is_empty() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "codex locator requires a session_id",
        ));
    }
    if locator.record_index.is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "codex locator requires a record_index",
        ));
    }
    let pointer = locator.pointer.as_deref().unwrap_or("");
    if !is_valid_json_pointer(pointer) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "codex locator requires a JSON pointer",
        ));
    }

    let path = resolve_codex_carrier(read_root, &reference.rel_path)?;
    ensure_codex_storage(source, read_root, &path)?;
    let reader = crate::codex_rollout_file::open_reader(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RecallError::new(codes::REF_LOCATOR_MISSING, "carrier does not exist")
        } else {
            RecallError::new(
                codes::REF_READ_FAILED,
                format!("carrier cannot be read: {err}"),
            )
        }
    })?;

    scan_codex_stream(reader, request)
}

/// The [`read_codex`] record scan over any decompressed rollout stream (a live
/// carrier, or stage-2 S5 archive evidence rebuilt into memory).
pub(crate) fn scan_codex_stream(
    mut reader: impl std::io::BufRead,
    request: &ReadRequest,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let locator = &reference.locator;
    let session_id = locator.session_id.as_deref().unwrap_or("");
    if session_id.is_empty() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "codex locator requires a session_id",
        ));
    }
    let record_index = locator.record_index.ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "codex locator requires a record_index",
        )
    })?;
    let pointer = locator.pointer.as_deref().unwrap_or("");
    if !is_valid_json_pointer(pointer) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "codex locator requires a JSON pointer",
        ));
    }

    let mut preceding: VecDeque<(Locator, String)> =
        VecDeque::with_capacity(request.context_before);
    let mut following: Vec<(Locator, String)> = Vec::new();
    let mut target: Option<(Locator, String)> = None;
    let mut target_row_sha256: Option<String> = None;

    let mut index: u64 = 0;
    let mut scan_bytes: u64 = 0;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        if target.is_some() && following.len() >= request.context_after {
            break;
        }
        buffer.clear();
        let line = read_line_bounded(
            &mut reader,
            &mut buffer,
            CODEX_SCAN_LIMIT_BYTES,
            // Before the record we must continue scanning; after it, a too-long
            // line simply ends the context window.
            target.is_none(),
        )
        .map_err(|err| RecallError::new(codes::REF_READ_FAILED, format!("read failed: {err}")))?;
        let (consumed, terminated, too_long) = match line {
            BoundedLine::Eof => break,
            BoundedLine::Line {
                consumed,
                terminated,
                too_long,
            } => (consumed, terminated, too_long),
        };
        scan_bytes += consumed;
        if target.is_none() && too_long {
            return Err(RecallError::new(
                codes::REF_READ_FAILED,
                "codex readback hit a record above the decompressed scan limit",
            ));
        }
        if too_long {
            break;
        }
        if target.is_none() && scan_bytes > CODEX_SCAN_LIMIT_BYTES {
            return Err(RecallError::new(
                codes::REF_READ_FAILED,
                "codex readback exceeded the decompressed scan limit before the record",
            ));
        }
        while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
            buffer.pop();
        }

        if target.is_none() {
            if index == record_index {
                let value: Value = serde_json::from_slice(&buffer).map_err(|_| {
                    RecallError::new(
                        codes::REF_READ_FAILED,
                        "the record at record_index is not readable JSON",
                    )
                })?;
                let RecordOutcome::Text(record) = project_record(&value) else {
                    return Err(RecallError::new(
                        codes::REF_LOCATOR_MISSING,
                        "the record at record_index does not project to text",
                    ));
                };
                if record.pointer != pointer {
                    return Err(RecallError::new(
                        codes::REF_LOCATOR_MISSING,
                        "locator pointer does not match the current record projection",
                    ));
                }
                if record.text.is_empty() {
                    return Err(RecallError::new(
                        codes::REF_OUT_OF_RANGE,
                        "the record projects to an empty text",
                    ));
                }
                verify_projection_range(
                    &record.text,
                    &reference.text_range,
                    &reference.fingerprint.range_sha256,
                )?;
                target_row_sha256 = Some(sha256_hex(&buffer));
                target = Some((
                    Locator::codex(
                        session_id,
                        record.message_id.as_deref(),
                        record_index,
                        &record.pointer,
                    ),
                    record.text,
                ));
            } else if request.context_before > 0 && !buffer.is_empty() {
                if let Ok(value) = serde_json::from_slice::<Value>(&buffer) {
                    if let RecordOutcome::Text(record) = project_record(&value) {
                        if !record.text.trim().is_empty() {
                            if preceding.len() == request.context_before {
                                preceding.pop_front();
                            }
                            preceding.push_back((
                                Locator::codex(
                                    session_id,
                                    record.message_id.as_deref(),
                                    index,
                                    &record.pointer,
                                ),
                                record.text,
                            ));
                        }
                    }
                }
            }
        } else if !buffer.is_empty() {
            if let Ok(value) = serde_json::from_slice::<Value>(&buffer) {
                if let RecordOutcome::Text(record) = project_record(&value) {
                    if !record.text.trim().is_empty() {
                        following.push((
                            Locator::codex(
                                session_id,
                                record.message_id.as_deref(),
                                index,
                                &record.pointer,
                            ),
                            record.text,
                        ));
                    }
                }
            }
        }

        index += 1;
        if !terminated {
            // A rollout is appended by a live agent: the final fragment may be
            // half a line. It is never a record; stop without failing.
            break;
        }
    }

    let Some(target) = target else {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "record_index is not present in the carrier",
        ));
    };

    let mut warnings = Vec::new();
    if let Some(expected) = &reference.fingerprint.row_sha256 {
        if target_row_sha256.as_deref() != Some(expected.as_str()) {
            warnings.push(WarningEntry::file_stale(
                Some(reference.source_id.clone()),
                Some(serde_json::json!({
                    "expected": expected,
                    "observed": target_row_sha256,
                })),
            ));
        }
    }

    let mut windows: Vec<(Locator, String)> =
        Vec::with_capacity(preceding.len() + 1 + following.len());
    windows.extend(preceding);
    windows.push(target);
    windows.extend(following.into_iter().take(request.context_after));
    let (segments, truncated) = assemble_codex_segments(windows, request.max_bytes)?;

    Ok(ReadOutcome {
        segments,
        warnings,
        truncated,
    })
}

/// `dsh-jsonl` readback: the locator addresses a logical line (decompressed,
/// 0-based) and one projected block inside the event at that line. The carrier
/// is re-read frame by frame and re-projected, so a changed projection, a
/// changed range or a shortened log fails closed.
fn read_dsh(
    source: &SourceConfig,
    request: &ReadRequest,
    read_root: &Path,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let locator = &reference.locator;
    let session_id = locator.session_id.as_deref().unwrap_or("");
    if session_id.is_empty() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "dsh locator requires a session_id",
        ));
    }
    if locator.record_index.is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "dsh locator requires a record_index",
        ));
    }
    let pointer = locator.pointer.as_deref().unwrap_or("");
    if !is_valid_json_pointer(pointer) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "dsh locator requires a JSON pointer",
        ));
    }
    if let Some(carrier) = locator.carrier.as_deref() {
        if carrier != reference.rel_path {
            return Err(RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "locator carrier does not match the ref rel_path",
            ));
        }
    }
    let path = safe_rollout_join(read_root, &reference.rel_path)
        .ok_or_else(|| RecallError::new(codes::REF_LOCATOR_MISSING, "unsafe rel_path"))?;
    if crate::recall::port::reader::rel_from_root(&source.root, &path).is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "carrier is outside the configured dsh root",
        ));
    }
    let root_str = read_root.to_string_lossy().to_string();
    if crate::recall::port::reader::rel_from_root(&root_str, &path).is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "carrier is outside the read root",
        ));
    }
    let log = dsh::open_log(&path, &reference.rel_path).map_err(|error| {
        if !path.exists() {
            RecallError::new(codes::REF_LOCATOR_MISSING, "carrier does not exist")
        } else {
            RecallError::new(codes::REF_READ_FAILED, error)
        }
    })?;
    scan_dsh_log(&log, request)
}

/// `hermes-sqlite-v1` readback: the locator is codex-shaped
/// (`record_index` = the message `id`, pointer `/content`); the projection is
/// the message content itself, so the read returns the whole row text and
/// re-verifies the range/row fingerprints against the live row.
fn read_hermes(
    source: &SourceConfig,
    request: &ReadRequest,
    read_root: &Path,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let configured_rel = source.db.clone().unwrap_or_default().replace('\\', "/");
    if configured_rel.is_empty() || reference.rel_path != configured_rel {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "hermes ref rel_path does not match the configured db",
        ));
    }
    let locator = &reference.locator;
    let session_id = locator.session_id.as_deref().unwrap_or("");
    if session_id.is_empty() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "hermes locator requires a session_id",
        ));
    }
    let Some(message_id) = locator.record_index else {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "hermes locator requires a record_index (the message id)",
        ));
    };
    let expected_message_id = message_id.to_string();
    if locator.message_id.as_deref() != Some(expected_message_id.as_str()) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "hermes locator message_id does not match record_index",
        ));
    }
    if locator.pointer.as_deref() != Some("/content") {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "hermes locator requires the /content pointer",
        ));
    }
    let readonly = hermes::open_db(source, read_root).map_err(|error| {
        RecallError::new(
            hermes::db_error_code(&error),
            format!("hermes db cannot be opened: {error}"),
        )
    })?;
    let messages = hermes::load_messages(&readonly, session_id).map_err(|error| {
        RecallError::new(
            hermes::db_error_code(&error),
            format!("hermes read failed: {error}"),
        )
    })?;
    let target = messages
        .iter()
        .position(|message| message.id as u64 == message_id)
        .ok_or_else(|| {
            RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "message is gone or no longer a projected row",
            )
        })?;
    let content = &messages[target].content;
    let expected_range = TextRange::from_span(content.as_bytes(), 0, content.len());
    if reference.text_range != expected_range {
        return Err(RecallError::new(
            codes::REF_STALE,
            "message text_range does not match the current projection",
        ));
    }
    let range_sha = sha256_hex(content.as_bytes());
    if reference.fingerprint.range_sha256 != range_sha {
        return Err(RecallError::new(
            codes::REF_STALE,
            "message content changed since the ref was issued",
        ));
    }
    if let Some(expected) = &reference.fingerprint.row_sha256 {
        let row_sha = sha256_hex(format!("{}:{}", message_id, content).as_bytes());
        if expected != &row_sha {
            return Err(RecallError::new(
                codes::REF_STALE,
                "message row changed since the ref was issued",
            ));
        }
    }
    let start = target.saturating_sub(request.context_before);
    let end = target
        .saturating_add(request.context_after)
        .saturating_add(1)
        .min(messages.len());
    let windows = messages[start..end]
        .iter()
        .map(|message| {
            (
                Locator::codex(
                    session_id,
                    Some(&message.id.to_string()),
                    message.id as u64,
                    "/content",
                ),
                message.content.clone(),
            )
        })
        .collect();
    let (segments, truncated) = assemble_codex_segments(windows, request.max_bytes)?;
    Ok(ReadOutcome {
        segments,
        warnings: Vec::new(),
        truncated,
    })
}

/// The [`read_dsh`] scan over a decoded logical log.
pub(crate) fn scan_dsh_log(
    log: &dsh::DshLog,
    request: &ReadRequest,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let locator = &reference.locator;
    let session_id = locator.session_id.as_deref().unwrap_or("");
    let record_index = locator.record_index.ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "dsh locator requires a record_index",
        )
    })?;
    let pointer = locator.pointer.as_deref().unwrap_or("");
    // One logical line occupies at most one window slot. A line can project
    // several units (for example an assistant message with two text blocks);
    // all of its non-empty units are kept as consecutive window entries, so a
    // `context_before: 1` window always covers exactly one preceding line.
    let mut preceding: VecDeque<Vec<(Locator, String)>> =
        VecDeque::with_capacity(request.context_before);
    let mut following: Vec<Vec<(Locator, String)>> = Vec::new();
    let mut target: Option<(Locator, String)> = None;
    let mut target_row_sha256: Option<String> = None;
    for line in log.lines() {
        if target.is_some() && following.len() >= request.context_after {
            break;
        }
        if line.bytes.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line.bytes) else {
            continue;
        };
        let event_seq = dsh::event_seq(&value);
        let units = match dsh::project_event(&value) {
            DshEventOutcome::Units(units) => units,
            _ => Vec::new(),
        };
        if line.index == record_index && target.is_none() {
            if let Some(expected) = locator.event_seq {
                if event_seq != Some(expected) {
                    return Err(RecallError::new(
                        codes::REF_LOCATOR_MISSING,
                        "locator event seq does not match the current event",
                    ));
                }
            }
            let unit = units
                .iter()
                .find(|unit| unit.pointer == pointer)
                .ok_or_else(|| {
                    RecallError::new(
                        codes::REF_LOCATOR_MISSING,
                        "locator pointer does not match the current event projection",
                    )
                })?;
            if unit.text.is_empty() {
                return Err(RecallError::new(
                    codes::REF_OUT_OF_RANGE,
                    "the projected unit is empty",
                ));
            }
            verify_projection_range(
                &unit.text,
                &reference.text_range,
                &reference.fingerprint.range_sha256,
            )?;
            target_row_sha256 = Some(sha256_hex(line.bytes));
            target = Some((
                Locator::dsh(
                    session_id,
                    record_index,
                    pointer,
                    event_seq,
                    Some(reference.rel_path.as_str()),
                ),
                unit.text.clone(),
            ));
        } else if target.is_none() {
            if request.context_before > 0 {
                let entries: Vec<(Locator, String)> = units
                    .iter()
                    .filter(|unit| !unit.text.trim().is_empty())
                    .map(|unit| {
                        (
                            Locator::dsh(
                                session_id,
                                line.index,
                                &unit.pointer,
                                event_seq,
                                Some(reference.rel_path.as_str()),
                            ),
                            unit.text.clone(),
                        )
                    })
                    .collect();
                if entries.is_empty() {
                    // A metadata-only line is transparent: it is not a slot.
                    continue;
                }
                if preceding.len() == request.context_before {
                    preceding.pop_front();
                }
                preceding.push_back(entries);
            }
        } else {
            let entries: Vec<(Locator, String)> = units
                .iter()
                .filter(|unit| !unit.text.trim().is_empty())
                .map(|unit| {
                    (
                        Locator::dsh(
                            session_id,
                            line.index,
                            &unit.pointer,
                            event_seq,
                            Some(reference.rel_path.as_str()),
                        ),
                        unit.text.clone(),
                    )
                })
                .collect();
            if entries.is_empty() {
                continue;
            }
            following.push(entries);
            if following.len() >= request.context_after {
                break;
            }
        }
    }

    let Some(target) = target else {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "record_index is not present in the carrier",
        ));
    };

    let mut warnings = Vec::new();
    if let Some(expected) = &reference.fingerprint.row_sha256 {
        if target_row_sha256.as_deref() != Some(expected.as_str()) {
            warnings.push(WarningEntry::file_stale(
                Some(reference.source_id.clone()),
                Some(serde_json::json!({
                    "expected": expected,
                    "observed": target_row_sha256,
                })),
            ));
        }
    }

    let mut windows: Vec<(Locator, String)> =
        Vec::with_capacity(preceding.iter().map(Vec::len).sum::<usize>() + 1);
    for line in preceding {
        windows.extend(line);
    }
    windows.push(target);
    for line in following.into_iter().take(request.context_after) {
        windows.extend(line);
    }
    let (segments, truncated) = assemble_codex_segments(windows, request.max_bytes)?;

    Ok(ReadOutcome {
        segments,
        warnings,
        truncated,
    })
}

/// `claude-jsonl-v1` readback: one transcript line is one record, and the
/// locator's JSON pointer selects one projected unit inside it.
///
/// `rel_path` is the carrier (`<project>/<session>.jsonl` or
/// `<project>/<session>/subagents/agent-*.jsonl`); the record is re-projected
/// and the unit at `pointer` must reproduce the referenced text exactly.
fn read_claude_code(
    source: &SourceConfig,
    request: &ReadRequest,
    read_root: &Path,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let locator = &reference.locator;
    let session_id = locator.session_id.as_deref().unwrap_or("");
    if session_id.is_empty() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "claude_code locator requires a session_id",
        ));
    }
    if locator.record_index.is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "claude_code locator requires a record_index",
        ));
    }
    let pointer = locator.pointer.as_deref().unwrap_or("");
    if !is_valid_json_pointer(pointer) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "claude_code locator requires a JSON pointer",
        ));
    }
    let path = safe_rollout_join(read_root, &reference.rel_path)
        .ok_or_else(|| RecallError::new(codes::REF_LOCATOR_MISSING, "unsafe rel_path"))?;
    // The carrier must stay inside the configured root (not merely inside the
    // read root), so a hand-written ref can never leave the source.
    if crate::recall::port::reader::rel_from_root(&source.root, &path).is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "carrier is outside the configured claude_code root",
        ));
    }
    let root_str = read_root.to_string_lossy().to_string();
    if crate::recall::port::reader::rel_from_root(&root_str, &path).is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "carrier is outside the read root",
        ));
    }
    let file = File::open(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RecallError::new(codes::REF_LOCATOR_MISSING, "carrier does not exist")
        } else {
            RecallError::new(
                codes::REF_READ_FAILED,
                format!("carrier cannot be read: {err}"),
            )
        }
    })?;
    let reader = std::io::BufReader::with_capacity(256 * 1024, file);
    scan_claude_stream(reader, request)
}

/// The [`read_claude_code`] record scan over any transcript stream (a live
/// carrier, or stage-2 S5 archive evidence rebuilt into memory).
pub(crate) fn scan_claude_stream(
    mut reader: impl std::io::BufRead,
    request: &ReadRequest,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let locator = &reference.locator;
    let session_id = locator.session_id.as_deref().unwrap_or("");
    if session_id.is_empty() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "claude_code locator requires a session_id",
        ));
    }
    let record_index = locator.record_index.ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "claude_code locator requires a record_index",
        )
    })?;
    let pointer = locator.pointer.as_deref().unwrap_or("");
    if !is_valid_json_pointer(pointer) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "claude_code locator requires a JSON pointer",
        ));
    }
    let mut preceding: VecDeque<(Locator, String)> =
        VecDeque::with_capacity(request.context_before);
    let mut following: Vec<(Locator, String)> = Vec::new();
    let mut target: Option<(Locator, String)> = None;
    let mut target_row_sha256: Option<String> = None;
    let mut buffer: Vec<u8> = Vec::new();
    let mut index: u64 = 0;
    let mut scan_bytes: u64 = 0;
    loop {
        if target.is_some() && following.len() >= request.context_after {
            break;
        }
        buffer.clear();
        let line = read_line_bounded(
            &mut reader,
            &mut buffer,
            claude_code::CLAUDE_SCAN_LIMIT_BYTES,
            target.is_none(),
        )
        .map_err(|err| RecallError::new(codes::REF_READ_FAILED, format!("read failed: {err}")))?;
        let (consumed, terminated, too_long) = match line {
            BoundedLine::Eof => break,
            BoundedLine::Line {
                consumed,
                terminated,
                too_long,
            } => (consumed, terminated, too_long),
        };
        scan_bytes = scan_bytes.saturating_add(consumed);
        if target.is_none() && too_long {
            return Err(RecallError::new(
                codes::REF_READ_FAILED,
                "claude_code readback hit a record above the decompressed scan limit",
            ));
        }
        if too_long {
            break;
        }
        if target.is_none() && scan_bytes > claude_code::CLAUDE_SCAN_LIMIT_BYTES {
            return Err(RecallError::new(
                codes::REF_READ_FAILED,
                "claude_code readback exceeded the scan limit before the record",
            ));
        }
        while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
            buffer.pop();
        }
        if !terminated {
            // Half-written final line of a live append: not a record.
            break;
        }
        if buffer.is_empty() {
            index += 1;
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&buffer) else {
            index += 1;
            continue;
        };
        let row_sha256 = sha256_hex(&buffer);
        let units = match claude_code::project_record(&value) {
            ClaudeRecordOutcome::Units(units) => units,
            _ => Vec::new(),
        };
        if index == record_index && target.is_none() {
            let unit = units
                .iter()
                .find(|unit| unit.pointer == pointer)
                .ok_or_else(|| {
                    RecallError::new(
                        codes::REF_LOCATOR_MISSING,
                        "locator pointer does not match the current record projection",
                    )
                })?;
            if unit.text.is_empty() {
                return Err(RecallError::new(
                    codes::REF_OUT_OF_RANGE,
                    "the projected unit is empty",
                ));
            }
            verify_projection_range(
                &unit.text,
                &reference.text_range,
                &reference.fingerprint.range_sha256,
            )?;
            target_row_sha256 = Some(row_sha256);
            target = Some((
                Locator::codex(
                    session_id,
                    locator.message_id.as_deref(),
                    record_index,
                    pointer,
                ),
                unit.text.clone(),
            ));
        } else if target.is_none() {
            if request.context_before > 0 {
                for unit in &units {
                    if unit.text.trim().is_empty() {
                        continue;
                    }
                    if preceding.len() == request.context_before {
                        preceding.pop_front();
                    }
                    preceding.push_back((
                        Locator::codex(
                            session_id,
                            locator.message_id.as_deref(),
                            index,
                            &unit.pointer,
                        ),
                        unit.text.clone(),
                    ));
                }
            }
        } else {
            for unit in &units {
                if unit.text.trim().is_empty() {
                    continue;
                }
                following.push((
                    Locator::codex(
                        session_id,
                        locator.message_id.as_deref(),
                        index,
                        &unit.pointer,
                    ),
                    unit.text.clone(),
                ));
                if following.len() >= request.context_after {
                    break;
                }
            }
        }
        index += 1;
    }

    let Some(target) = target else {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "record_index is not present in the carrier",
        ));
    };

    let mut warnings = Vec::new();
    if let Some(expected) = &reference.fingerprint.row_sha256 {
        if target_row_sha256.as_deref() != Some(expected.as_str()) {
            warnings.push(WarningEntry::file_stale(
                Some(reference.source_id.clone()),
                Some(serde_json::json!({
                    "expected": expected,
                    "observed": target_row_sha256,
                })),
            ));
        }
    }

    let mut windows: Vec<(Locator, String)> =
        Vec::with_capacity(preceding.len() + 1 + following.len());
    windows.extend(preceding);
    windows.push(target);
    windows.extend(following.into_iter().take(request.context_after));
    let (segments, truncated) = assemble_codex_segments(windows, request.max_bytes)?;
    Ok(ReadOutcome {
        segments,
        warnings,
        truncated,
    })
}

fn read_archive(
    source: &SourceConfig,
    request: &ReadRequest,
    effective_domain: &str,
    explicit: bool,
) -> Result<ReadOutcome, RecallError> {
    let reference = &request.reference;
    let locator = &reference.locator;
    let record_sha = locator.record_sha256.as_deref().ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "archive locator requires record_sha256",
        )
    })?;
    if !archive_provider::is_sha256(record_sha) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "archive locator record_sha256 must be 64 lowercase hex digits",
        ));
    }
    let projection_offset = locator.projection_offset.ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "archive locator requires projection_offset",
        )
    })?;
    if projection_offset != reference.text_range.byte_start {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "locator projection_offset does not match text_range.byte_start",
        ));
    }
    let object_sha =
        archive_provider::object_sha_from_rel_path(&reference.rel_path).ok_or_else(|| {
            RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "archive rel_path is not a real object address",
            )
        })?;

    let archive = archive_provider::open(Path::new(&source.root)).map_err(map_archive_open)?;
    let units = archive_provider::units(&archive).map_err(map_archive_error)?;
    // A ref names its carrier: the record must be committed in exactly the
    // object `rel_path` points at. Two carriers holding the same record line are
    // two distinct refs, and each one resolves to its own source window.
    let position = units
        .iter()
        .position(|unit| unit.record.sha256 == record_sha && unit.record.object == object_sha)
        .ok_or_else(|| {
            RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "record_sha256 is not committed in the object named by rel_path",
            )
        })?;
    let unit = &units[position];
    match classify_access(&unit.record.access, effective_domain, explicit) {
        AccessDecision::Readable => {}
        decision => {
            let code = decision
                .rejection()
                .map(|(_, code)| code)
                .unwrap_or(codes::DOMAIN_DENIED);
            return Err(RecallError::new(
                code,
                format!(
                    "archive record is not readable for domain {effective_domain:?} \
                     (missing or non-matching access labels fail closed)"
                ),
            ));
        }
    }

    // One source only: context never crosses a physical source boundary.
    let source_units: Vec<&Unit> = units
        .iter()
        .filter(|candidate| candidate.source == unit.source)
        .collect();
    let index = source_units
        .iter()
        .position(|candidate| {
            candidate.record.sha256 == record_sha && candidate.record.object == object_sha
        })
        .expect("record present in its own source");

    // P4: the whole turn, assembled from record-level refs. It never crosses a
    // physical source, a turn boundary or an invisible (denied) record.
    if request.context == ContextMode::Turn {
        // A DSH transcript has no turn segmentation this build can prove: the
        // archive stores decoded JSONL lines, not the framework's logical turn
        // graph. Refuse explicitly instead of returning a guessed window.
        if unit.provider == "dsh" {
            return Err(RecallError::invalid(
                "context",
                "unsupported_for_source",
                "context \"turn\" cannot be defined for archived dsh records; \
                 read the record (with context_before/context_after) instead",
            ));
        }
        return assemble_turn(
            &archive,
            &source_units,
            index,
            reference,
            request.max_bytes,
            effective_domain,
            explicit,
        );
    }

    let start = index.saturating_sub(request.context_before);
    let end = (index + 1 + request.context_after).min(source_units.len());

    let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let target_text = unit_text(&archive, effective_domain, unit, &mut cache)?;
    let range = reference.text_range;
    // Byte-wise validation first: `text_range` must be a non-empty, in-bounds,
    // character-aligned span of the decoded projection. Without the char-boundary
    // check a hand-written ref could panic a string slice.
    let target_slice = {
        let bytes = target_text.as_bytes();
        if range.byte_start >= range.byte_end || range.byte_end > bytes.len() as u64 {
            return Err(RecallError::new(
                codes::REF_OUT_OF_RANGE,
                "text_range must cover at least one byte inside the decoded record projection",
            ));
        }
        let start = range.byte_start as usize;
        let end = range.byte_end as usize;
        if sha256_hex(&bytes[start..end]) != reference.fingerprint.range_sha256 {
            return Err(RecallError::new(
                codes::REF_STALE,
                "range_sha256 does not match the decoded record projection",
            ));
        }
        std::str::from_utf8(&bytes[start..end])
            .map_err(|_| {
                RecallError::new(
                    codes::REF_OUT_OF_RANGE,
                    "text_range does not align to a UTF-8 character boundary",
                )
            })?
            .to_string()
    };

    // Reading order is preserved, but the target always gets budget room: a
    // preceding context segment can never push the referenced record out.
    let mut pre: Vec<(Locator, TextRange, String)> = Vec::new();
    for candidate in &source_units[start..index] {
        // N2: a context record must pass its own record-level predicate, not just
        // the object-level intersection. A neighbour from another domain is
        // omitted instead of leaking into the window.
        if !matches!(
            classify_access(&candidate.record.access, effective_domain, explicit),
            AccessDecision::Readable
        ) {
            continue;
        }
        // Context is best-effort: an unreadable neighbour is omitted instead of
        // failing an otherwise valid read.
        let Ok(text) = unit_text(&archive, effective_domain, candidate, &mut cache) else {
            continue;
        };
        let bytes = text.as_bytes();
        pre.push((
            Locator::archive(&candidate.record.sha256, 0),
            TextRange::from_span(bytes, 0, bytes.len()),
            text,
        ));
    }
    let mut post: Vec<(Locator, TextRange, String)> = Vec::new();
    for candidate in &source_units[index + 1..end] {
        if !matches!(
            classify_access(&candidate.record.access, effective_domain, explicit),
            AccessDecision::Readable
        ) {
            continue;
        }
        let Ok(text) = unit_text(&archive, effective_domain, candidate, &mut cache) else {
            continue;
        };
        let bytes = text.as_bytes();
        post.push((
            Locator::archive(&candidate.record.sha256, 0),
            TextRange::from_span(bytes, 0, bytes.len()),
            text,
        ));
    }
    let target_len = target_slice.len();
    let mut segments = Vec::new();
    let mut remaining = request.max_bytes;
    let mut truncated = false;
    // Emit preceding context only while at least the whole target still fits.
    for (locator, segment_range, text) in pre {
        if remaining <= target_len || remaining - target_len < text.len() {
            truncated = true;
            break;
        }
        remaining -= text.len();
        segments.push(Segment {
            locator,
            text_range: segment_range,
            range_sha256: sha256_hex(text.as_bytes()),
            text,
            reference: None,
        });
    }
    // The target record is always emitted (truncated only if it alone exceeds
    // the whole byte budget).
    if target_len <= remaining {
        remaining -= target_len;
        segments.push(Segment {
            locator: reference.locator.clone(),
            text_range: range,
            range_sha256: sha256_hex(target_slice.as_bytes()),
            text: target_slice,
            reference: None,
        });
    } else {
        let cut = crate::recall::text::utf8_floor(&target_slice, remaining);
        if cut == 0 {
            return Err(RecallError::new(
                codes::REF_OUT_OF_RANGE,
                "max_bytes cannot hold one character of the referenced record",
            ));
        }
        let prefix = &target_slice[..cut];
        let start = range.byte_start as usize;
        segments.push(Segment {
            locator: reference.locator.clone(),
            text_range: TextRange::from_span(target_text.as_bytes(), start, start + cut),
            range_sha256: sha256_hex(prefix.as_bytes()),
            text: prefix.to_string(),
            reference: None,
        });
        truncated = true;
        remaining = 0;
    }
    // Following context takes whatever budget is left.
    for (locator, segment_range, text) in post {
        if remaining == 0 {
            truncated = true;
            break;
        }
        if text.len() <= remaining {
            remaining -= text.len();
            segments.push(Segment {
                locator,
                text_range: segment_range,
                range_sha256: sha256_hex(text.as_bytes()),
                text,
                reference: None,
            });
        } else {
            let cut = crate::recall::text::utf8_floor(&text, remaining);
            if cut == 0 {
                truncated = true;
                break;
            }
            let prefix = &text[..cut];
            segments.push(Segment {
                locator,
                text_range: TextRange::from_span(prefix.as_bytes(), 0, prefix.len()),
                range_sha256: sha256_hex(prefix.as_bytes()),
                text: prefix.to_string(),
                reference: None,
            });
            truncated = true;
            break;
        }
    }
    if segments.is_empty() {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "readback produced no segment",
        ));
    }

    Ok(ReadOutcome {
        segments,
        warnings: Vec::new(),
        truncated,
    })
}

/// Result of "project this record if it is visible in this turn window".
///
/// `None` means the record must not be crossed: its access label denies the
/// effective domain, the object is unreadable, it has no text projection, or it
/// is a sidechain transcript. A turn assembly therefore stops there and reports
/// `truncated` instead of stitching two turns together.
fn turn_visible_text(
    archive: &crate::archive::Archive,
    domain: &str,
    explicit: bool,
    unit: &Unit,
    cache: &mut BTreeMap<String, Vec<u8>>,
) -> Option<String> {
    if !matches!(
        classify_access(&unit.record.access, domain, explicit),
        AccessDecision::Readable
    ) {
        return None;
    }
    let text = unit_text(archive, domain, unit, cache).ok()?;
    if crate::recall::graph::is_sidechain_record(&unit.provider, &text) {
        return None;
    }
    Some(text)
}

/// Verify that the request range is a non-empty, in-bounds, character-aligned
/// span of the decoded record whose bytes hash to `range_sha256`.
fn verify_archive_range(text: &str, reference: &HitRef) -> Result<(), RecallError> {
    let bytes = text.as_bytes();
    let range = reference.text_range;
    if range.byte_start >= range.byte_end || range.byte_end > bytes.len() as u64 {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "text_range must cover at least one byte inside the decoded record projection",
        ));
    }
    let start = range.byte_start as usize;
    let end = range.byte_end as usize;
    if sha256_hex(&bytes[start..end]) != reference.fingerprint.range_sha256 {
        return Err(RecallError::new(
            codes::REF_STALE,
            "range_sha256 does not match the decoded record projection",
        ));
    }
    if std::str::from_utf8(&bytes[start..end]).is_err() {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "text_range does not align to a UTF-8 character boundary",
        ));
    }
    Ok(())
}

/// One `archive-record-v1` segment for a whole committed record.
fn record_segment(unit: &Unit, text: &str, reference: &HitRef) -> Result<Segment, RecallError> {
    let rel_path = archive_provider::object_rel_path(&unit.record.object).ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "record object has no content-addressed rel_path",
        )
    })?;
    let bytes = text.as_bytes();
    let text_range = TextRange::from_span(bytes, 0, bytes.len());
    let range_sha256 = sha256_hex(bytes);
    let locator = Locator::archive(&unit.record.sha256, 0);
    Ok(Segment {
        locator: locator.clone(),
        text_range,
        range_sha256: range_sha256.clone(),
        text: text.to_string(),
        reference: Some(HitRef {
            source_id: reference.source_id.clone(),
            source_kind: reference.source_kind.clone(),
            root_sha256: reference.root_sha256.clone(),
            rel_path,
            schema_gen: SCHEMA_ARCHIVE_RECORD_V1.to_string(),
            locator,
            fingerprint: Fingerprint {
                range_sha256,
                file_sha256: None,
                row_sha256: None,
            },
            text_range,
        }),
    })
}

/// `context: "turn"` assembly (§3.5): one user message plus every following
/// assistant/tool record until the next user message.
///
/// Invariants:
///
/// * only records of **this physical source** participate (no cross-session
///   stitching);
/// * every candidate must pass its own record-level access predicate, so a
///   denied record never appears and never silently splits the turn into two
///   legs that look contiguous;
/// * a subagent/sidechain session is never expanded;
/// * the referenced record always gets budget room, the remaining records are
///   added by increasing distance from it, and any omission sets `truncated`.
fn assemble_turn(
    archive: &crate::archive::Archive,
    source_units: &[&Unit],
    index: usize,
    reference: &HitRef,
    max_bytes: usize,
    effective_domain: &str,
    explicit: bool,
) -> Result<ReadOutcome, RecallError> {
    let target = source_units[index];
    let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let target_text = unit_text(archive, effective_domain, target, &mut cache)?;
    // Turn assembly replaces the referenced span with the whole record, but the
    // ref it was called with is still verified: a stale or forged range is
    // never silently widened.
    verify_archive_range(&target_text, reference)?;

    // A subagent session, or a sidechain record inside an otherwise main file,
    // has no provable turn ownership: only the addressed record is returned.
    let target_sidechain =
        crate::recall::graph::is_sidechain_record(&target.provider, &target_text);
    if target_sidechain {
        return Ok(ReadOutcome {
            segments: vec![record_segment(target, &target_text, reference)?],
            warnings: Vec::new(),
            truncated: true,
        });
    }

    // A Codex `session_meta.payload.source.subagent` marker declares the whole
    // physical source a subagent session; its ownership is unknown to this
    // reader, so no turn is assembled around it.
    let subagent = source_units.iter().take(SUBAGENT_SCAN_RECORDS).any(|unit| {
        match unit_text(archive, effective_domain, unit, &mut cache) {
            Ok(text) => crate::recall::graph::is_subagent_session_record(&unit.provider, &text),
            Err(_) => false,
        }
    });
    if subagent {
        return Ok(ReadOutcome {
            segments: vec![record_segment(target, &target_text, reference)?],
            warnings: Vec::new(),
            truncated: true,
        });
    }

    let mut texts: BTreeMap<usize, String> = BTreeMap::new();
    texts.insert(index, target_text.clone());
    let mut picked: Vec<usize> = vec![index];
    let mut truncated = false;

    // Backwards to the user record that opened this turn.
    if !crate::recall::semantic::units::is_user_turn_record(&target.provider, &target_text) {
        let mut cursor = index;
        let mut scanned = 0usize;
        loop {
            if cursor == 0 {
                break;
            }
            if scanned >= crate::recall::graph::TURN_CONTEXT_MAX_RECORDS {
                truncated = true;
                break;
            }
            cursor -= 1;
            scanned += 1;
            let candidate = source_units[cursor];
            let Some(text) =
                turn_visible_text(archive, effective_domain, explicit, candidate, &mut cache)
            else {
                truncated = true;
                break;
            };
            // Metadata-only records are transparent: they never become segments
            // and they never end the turn.
            if crate::recall::semantic::units::extract(&candidate.provider, &text).is_empty() {
                continue;
            }
            let opens =
                crate::recall::semantic::units::is_user_turn_record(&candidate.provider, &text);
            texts.insert(cursor, text);
            picked.push(cursor);
            if opens {
                break;
            }
        }
    }

    // Forwards until the next user record (exclusive) or an invisible record.
    let mut cursor = index;
    let mut scanned = 0usize;
    loop {
        if cursor + 1 >= source_units.len() {
            break;
        }
        if scanned >= crate::recall::graph::TURN_CONTEXT_MAX_RECORDS {
            truncated = true;
            break;
        }
        cursor += 1;
        scanned += 1;
        let candidate = source_units[cursor];
        let Some(text) =
            turn_visible_text(archive, effective_domain, explicit, candidate, &mut cache)
        else {
            truncated = true;
            break;
        };
        if crate::recall::semantic::units::extract(&candidate.provider, &text).is_empty() {
            // Metadata-only record: transparent, never a segment.
            continue;
        }
        if crate::recall::semantic::units::is_user_turn_record(&candidate.provider, &text) {
            // The next turn starts here; it is never part of this one.
            break;
        }
        texts.insert(cursor, text);
        picked.push(cursor);
    }

    picked.sort_unstable();
    picked.dedup();
    if picked.len() > crate::recall::graph::TURN_CONTEXT_MAX_SEGMENTS {
        truncated = true;
        let mut by_distance: Vec<usize> = picked
            .iter()
            .copied()
            .filter(|candidate| *candidate != index)
            .collect();
        by_distance.sort_by_key(|candidate| (candidate.abs_diff(index), *candidate));
        let keep = crate::recall::graph::TURN_CONTEXT_MAX_SEGMENTS.saturating_sub(1);
        let mut kept: Vec<usize> = by_distance.into_iter().take(keep).collect();
        kept.push(index);
        kept.sort_unstable();
        picked = kept;
    }

    let target_len = target_text.len();
    if target_len > max_bytes {
        // The target alone exceeds the budget: emit its prefix and nothing else.
        let cut = crate::recall::text::utf8_floor(&target_text, max_bytes);
        if cut == 0 {
            return Err(RecallError::new(
                codes::REF_OUT_OF_RANGE,
                "max_bytes cannot hold one character of the referenced record",
            ));
        }
        return Ok(ReadOutcome {
            segments: vec![record_segment(target, &target_text[..cut], reference)?],
            warnings: Vec::new(),
            truncated: true,
        });
    }

    // The target is placed first in the budget; the rest is filled by distance.
    let mut selected: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    selected.insert(index);
    let mut remaining = max_bytes - target_len;
    let mut rest: Vec<usize> = picked
        .iter()
        .copied()
        .filter(|candidate| *candidate != index)
        .collect();
    rest.sort_by_key(|candidate| (candidate.abs_diff(index), *candidate));
    for candidate in rest {
        let Some(text) = texts.get(&candidate) else {
            truncated = true;
            continue;
        };
        if text.len() <= remaining {
            remaining -= text.len();
            selected.insert(candidate);
        } else {
            truncated = true;
        }
    }

    let mut segments = Vec::new();
    for candidate in selected {
        let unit = source_units[candidate];
        let text = texts
            .get(&candidate)
            .expect("every selected record was projected");
        segments.push(record_segment(unit, text, reference)?);
    }
    if segments.is_empty() {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "readback produced no segment",
        ));
    }

    Ok(ReadOutcome {
        segments,
        warnings: Vec::new(),
        truncated,
    })
}

fn load_object<'a>(
    archive: &crate::archive::Archive,
    domain: &str,
    sha256: &str,
    cache: &'a mut BTreeMap<String, Vec<u8>>,
) -> Result<&'a [u8], RecallError> {
    if !cache.contains_key(sha256) {
        let bytes =
            archive_provider::read_object(archive, domain, sha256).map_err(map_archive_error)?;
        cache.insert(sha256.to_string(), bytes);
    }
    Ok(cache
        .get(sha256)
        .map(|bytes| bytes.as_slice())
        .expect("inserted above"))
}

fn unit_text(
    archive: &crate::archive::Archive,
    domain: &str,
    unit: &Unit,
    cache: &mut BTreeMap<String, Vec<u8>>,
) -> Result<String, RecallError> {
    let start = usize::try_from(unit.record.offset)
        .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "record offset overflows"))?;
    let len = usize::try_from(unit.record.len)
        .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "record len overflows"))?;
    let object = load_object(archive, domain, &unit.record.object, cache)?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| RecallError::new(codes::REF_READ_FAILED, "record span overflows"))?;
    if end > object.len() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "record span exceeds its object",
        ));
    }
    let slice = &object[start..end];
    if sha256_hex(slice) != unit.record.sha256 {
        return Err(RecallError::new(
            codes::REF_READ_FAILED,
            "record bytes do not match record_sha256",
        ));
    }
    archive_provider::project_record(slice).ok_or_else(|| {
        RecallError::new(
            codes::REF_READ_FAILED,
            "record has no UTF-8 text projection",
        )
    })
}

fn map_archive_open(failure: archive_provider::ArchiveFailure) -> RecallError {
    match failure.reason {
        "archive_root_not_initialized" => {
            RecallError::new(codes::REF_LOCATOR_MISSING, failure.message)
        }
        _ => RecallError::new(codes::REF_READ_FAILED, failure.message),
    }
}

fn map_archive_error(error: crate::archive::ArchiveError) -> RecallError {
    match error {
        crate::archive::ArchiveError::AccessDenied(message) => {
            RecallError::new(codes::DOMAIN_DENIED, message)
        }
        crate::archive::ArchiveError::NotFound(message) => {
            RecallError::new(codes::REF_LOCATOR_MISSING, message)
        }
        crate::archive::ArchiveError::Integrity(message)
        | crate::archive::ArchiveError::Corrupt(message) => {
            RecallError::new(codes::REF_READ_FAILED, message)
        }
        other => RecallError::new(codes::REF_READ_FAILED, other.to_string()),
    }
}

fn verify_projection_range(
    projection: &str,
    range: &TextRange,
    expected_sha256: &str,
) -> Result<(), RecallError> {
    let bytes = projection.as_bytes();
    if range.byte_start >= range.byte_end || range.byte_end > bytes.len() as u64 {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "text_range must cover at least one byte inside the record projection",
        ));
    }
    let start = range.byte_start as usize;
    let end = range.byte_end as usize;
    if TextRange::from_span(bytes, start, end) != *range {
        return Err(RecallError::new(
            codes::REF_STALE,
            "text_range line coordinates do not match the current projection",
        ));
    }
    if sha256_hex(&bytes[start..end]) != expected_sha256 {
        return Err(RecallError::new(
            codes::REF_STALE,
            "range_sha256 does not match the current projection",
        ));
    }
    Ok(())
}

fn assemble_codex_segments(
    windows: Vec<(Locator, String)>,
    max_bytes: usize,
) -> Result<(Vec<Segment>, bool), RecallError> {
    let mut segments = Vec::new();
    let mut remaining = max_bytes;
    let mut truncated = false;
    for (locator, text) in windows {
        if remaining == 0 {
            truncated = true;
            break;
        }
        let bytes = text.as_bytes();
        let len = bytes.len();
        if len <= remaining {
            let text_range = TextRange::from_span(bytes, 0, len);
            let range_sha256 = sha256_hex(bytes);
            segments.push(Segment {
                locator,
                text_range,
                range_sha256,
                text,
                reference: None,
            });
            remaining -= len;
        } else {
            let cut = utf8_floor(&text, remaining);
            if cut == 0 {
                truncated = true;
                break;
            }
            let prefix = &text[..cut];
            segments.push(Segment {
                locator,
                text_range: TextRange::from_span(prefix.as_bytes(), 0, prefix.len()),
                range_sha256: sha256_hex(prefix.as_bytes()),
                text: prefix.to_string(),
                reference: None,
            });
            truncated = true;
            break;
        }
    }
    if segments.is_empty() {
        return Err(RecallError::new(
            codes::REF_OUT_OF_RANGE,
            "readback produced no segment",
        ));
    }
    Ok((segments, truncated))
}

/// A Codex ref is only readable from the configured `storage` tree, not merely
/// from inside `root`. This keeps an unconfigured `archived_sessions` tree
/// unreachable through a hand-written ref even though it sits under the home.
fn ensure_codex_storage(
    source: &SourceConfig,
    read_root: &Path,
    carrier: &Path,
) -> Result<(), RecallError> {
    let storage = codex_storage_dir(source, read_root).ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "codex storage path cannot be resolved",
        )
    })?;
    let storage_string = storage.to_string_lossy().to_string();
    if crate::recall::port::reader::rel_from_root(&storage_string, carrier).is_none() {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "carrier is outside the configured codex storage tree",
        ));
    }
    Ok(())
}

fn resolve_codex_carrier(read_root: &Path, rel_path: &str) -> Result<PathBuf, RecallError> {
    let plain = safe_rollout_join(read_root, rel_path)
        .ok_or_else(|| RecallError::new(codes::REF_LOCATOR_MISSING, "unsafe rel_path"))?;
    if plain.is_file() {
        return Ok(plain);
    }
    if let Some(sibling) = crate::codex_rollout_file::sibling(&plain) {
        // The sibling is a new final path component; a link there is refused
        // exactly like one in the plain path.
        if std::fs::symlink_metadata(&sibling)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "carrier is a symbolic link",
            ));
        }
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Err(RecallError::new(
        codes::REF_LOCATOR_MISSING,
        "carrier does not exist in either plain or compressed form",
    ))
}

/// Lexically contained join that refuses every symbolic-link component below
/// the read root. Enumeration already skips links; readback must agree instead
/// of following a link out of the configured tree.
fn safe_rollout_join(root: &Path, rel_path: &str) -> Option<PathBuf> {
    if rel_path.is_empty()
        || rel_path.starts_with('/')
        || rel_path.starts_with('\\')
        || rel_path.contains(':')
    {
        return None;
    }
    let mut path = PathBuf::from(root);
    for component in rel_path.split(['/', '\\']) {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return None,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(path)
}

fn parse_ref(value: &Value) -> Result<HitRef, RecallError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid_type("ref", "object"))?;
    reject_unknown(map, &REF_KEYS, "ref")?;
    let source_id = required_string(map, "source_id", "ref")?;
    let source_kind = required_string(map, "source_kind", "ref")?;
    let root_sha = required_string(map, "root_sha256", "ref")?;
    let rel_path = required_string(map, "rel_path", "ref")?;
    let schema_gen = required_string(map, "schema_gen", "ref")?;
    if !ALL_SCHEMA_GENS.contains(&schema_gen.as_str()) {
        return Err(RecallError::invalid(
            "ref.schema_gen",
            "unsupported_schema_gen",
            "unknown schema_gen",
        ));
    }
    let locator = parse_locator(
        map.get("locator").ok_or_else(|| missing("ref.locator"))?,
        &schema_gen,
    )?;
    let fingerprint = parse_fingerprint(
        map.get("fingerprint")
            .ok_or_else(|| missing("ref.fingerprint"))?,
    )?;
    if schema_gen == SCHEMA_SQLITE_V2
        && locator.part_id.is_some()
        && fingerprint.row_sha256.as_deref().is_none_or(str::is_empty)
    {
        return Err(missing("ref.fingerprint.row_sha256"));
    }
    let text_range = parse_text_range(
        map.get("text_range")
            .ok_or_else(|| missing("ref.text_range"))?,
    )?;
    Ok(HitRef {
        source_id,
        source_kind,
        root_sha256: root_sha,
        rel_path,
        schema_gen,
        locator,
        fingerprint,
        text_range,
    })
}

fn parse_locator(value: &Value, schema_gen: &str) -> Result<Locator, RecallError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid_type("ref.locator", "object"))?;
    // Stage-2 S5: the archive-fallback marker is optional on the generations
    // the fallback produces (sqlite-v2 / codex-rollout-v1 / claude-jsonl-v1).
    // Every other generation keeps its exact key set.
    let marker = match map.get("archive_fallback") {
        None => None,
        Some(value) => {
            if !matches!(
                schema_gen,
                SCHEMA_SQLITE_V2
                    | SCHEMA_SQLITE_V1
                    | SCHEMA_CODEX_ROLLOUT_V1
                    | SCHEMA_CLAUDE_JSONL_V1
                    | SCHEMA_DSH_JSONL
            ) {
                return Err(RecallError::invalid(
                    "ref.locator.archive_fallback",
                    "unknown_key",
                    format!("unknown request key: ref.locator.archive_fallback (schema_gen {schema_gen})"),
                ));
            }
            let marker = value
                .as_object()
                .ok_or_else(|| invalid_type("ref.locator.archive_fallback", "object"))?;
            reject_unknown(
                marker,
                &["record_sha256", "deleted_at_ms"],
                "ref.locator.archive_fallback",
            )?;
            let record_sha256 =
                optional_string(marker, "record_sha256", "ref.locator.archive_fallback")?;
            if let Some(sha) = &record_sha256 {
                if !crate::recall::providers::archive::is_sha256(sha) {
                    return Err(RecallError::invalid(
                        "ref.locator.archive_fallback.record_sha256",
                        "invalid_sha256",
                        "record_sha256 must be 64 lowercase hex digits",
                    ));
                }
            }
            let deleted_at_ms = match marker.get("deleted_at_ms") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_i64().ok_or_else(|| {
                    RecallError::invalid(
                        "ref.locator.archive_fallback.deleted_at_ms",
                        "invalid_type",
                        "deleted_at_ms must be an integer (epoch milliseconds)",
                    )
                })?),
            };
            Some(crate::recall::refs::ArchiveLocator {
                record_sha256,
                deleted_at_ms,
            })
        }
    };
    let mut locator = locator_without_marker(value, schema_gen)?;
    locator.archive_fallback = marker;
    Ok(locator)
}

fn locator_without_marker(value: &Value, schema_gen: &str) -> Result<Locator, RecallError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid_type("ref.locator", "object"))?;
    match schema_gen {
        SCHEMA_MD_V1 => {
            reject_unknown(map, &[], "ref.locator")?;
            Ok(Locator::empty())
        }
        crate::recall::contract::SCHEMA_DOCS_MD_V1 => {
            // A docs ref addresses one heading chunk: the pointer names its
            // start byte, `text_range` its length.
            reject_unknown(map, &["pointer"], "ref.locator")?;
            let pointer = required_string(map, "pointer", "ref.locator")?;
            let Some(rest) = pointer.strip_prefix("/chunk/") else {
                return Err(RecallError::invalid(
                    "ref.locator.pointer",
                    "invalid_pointer",
                    "docs pointer must be /chunk/<byte offset>",
                ));
            };
            if rest.parse::<u64>().is_err() {
                return Err(RecallError::invalid(
                    "ref.locator.pointer",
                    "invalid_pointer",
                    "docs pointer must be /chunk/<byte offset>",
                ));
            }
            let mut locator = Locator::empty();
            locator.pointer = Some(pointer);
            Ok(locator)
        }
        SCHEMA_JSON_LEGACY_V1 | SCHEMA_SQLITE_V1 => {
            reject_unknown(
                map,
                &["session_id", "message_id", "part_id", "archive_fallback"],
                "ref.locator",
            )?;
            Ok(Locator::v1(
                &required_string(map, "session_id", "ref.locator")?,
                &required_string(map, "message_id", "ref.locator")?,
                &required_string(map, "part_id", "ref.locator")?,
            ))
        }
        SCHEMA_SQLITE_V2 => {
            reject_unknown(
                map,
                &[
                    "session_id",
                    "session_message_id",
                    "part_id",
                    "pointer",
                    "archive_fallback",
                ],
                "ref.locator",
            )?;
            let pointer = required_string(map, "pointer", "ref.locator")?;
            let part_id = if map.contains_key("part_id") {
                Some(required_string(map, "part_id", "ref.locator")?)
            } else {
                None
            };
            if part_id.is_some() && pointer != "/text" {
                return Err(RecallError::invalid(
                    "ref.locator.pointer",
                    "invalid_pointer",
                    "session_part pointer must be /text",
                ));
            }
            if parse_content_pointer(&pointer).is_none() {
                return Err(RecallError::invalid(
                    "ref.locator.pointer",
                    "invalid_pointer",
                    "pointer must be /text or /content/{i}",
                ));
            }
            let session_id = required_string(map, "session_id", "ref.locator")?;
            let message_id = required_string(map, "session_message_id", "ref.locator")?;
            Ok(match part_id {
                Some(part_id) => Locator::v2_part(&session_id, &message_id, &part_id),
                None => Locator::v2(&session_id, &message_id, &pointer),
            })
        }
        SCHEMA_CODEX_ROLLOUT_V1 | crate::recall::contract::SCHEMA_HERMES_SQLITE_V1 => {
            reject_unknown(
                map,
                &[
                    "session_id",
                    "message_id",
                    "record_index",
                    "pointer",
                    "archive_fallback",
                ],
                "ref.locator",
            )?;
            let pointer = required_string(map, "pointer", "ref.locator")?;
            if !is_valid_json_pointer(&pointer) {
                return Err(RecallError::invalid(
                    "ref.locator.pointer",
                    "invalid_pointer",
                    "pointer must be a JSON pointer starting with /",
                ));
            }
            let record_index = required_u64(map, "record_index", "ref.locator")?;
            let message_id = optional_string(map, "message_id", "ref.locator")?;
            Ok(Locator::codex(
                &required_string(map, "session_id", "ref.locator")?,
                message_id.as_deref(),
                record_index,
                &pointer,
            ))
        }
        SCHEMA_CLAUDE_JSONL_V1 => {
            // Same coordinate shape as a Codex rollout: one JSONL line, one
            // JSON pointer inside that line.
            reject_unknown(
                map,
                &[
                    "session_id",
                    "message_id",
                    "record_index",
                    "pointer",
                    "archive_fallback",
                ],
                "ref.locator",
            )?;
            let pointer = required_string(map, "pointer", "ref.locator")?;
            if !is_valid_json_pointer(&pointer) {
                return Err(RecallError::invalid(
                    "ref.locator.pointer",
                    "invalid_pointer",
                    "pointer must be a JSON pointer starting with /",
                ));
            }
            let record_index = required_u64(map, "record_index", "ref.locator")?;
            let message_id = optional_string(map, "message_id", "ref.locator")?;
            Ok(Locator::codex(
                &required_string(map, "session_id", "ref.locator")?,
                message_id.as_deref(),
                record_index,
                &pointer,
            ))
        }
        SCHEMA_DSH_JSONL => {
            // Logical line + block pointer; the event `seq` is an optional
            // coordinate (a hand-written ref without it still addresses the
            // unit).
            reject_unknown(
                map,
                &[
                    "session_id",
                    "record_index",
                    "pointer",
                    "event_seq",
                    "carrier",
                    "archive_fallback",
                ],
                "ref.locator",
            )?;
            let pointer = required_string(map, "pointer", "ref.locator")?;
            if !is_valid_json_pointer(&pointer) {
                return Err(RecallError::invalid(
                    "ref.locator.pointer",
                    "invalid_pointer",
                    "pointer must be a JSON pointer starting with /",
                ));
            }
            let record_index = required_u64(map, "record_index", "ref.locator")?;
            let event_seq = match map.get("event_seq") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    RecallError::invalid(
                        "ref.locator.event_seq",
                        "invalid_type",
                        "event_seq must be a non-negative integer",
                    )
                })?),
            };
            let carrier = optional_string(map, "carrier", "ref.locator")?;
            Ok(Locator::dsh(
                &required_string(map, "session_id", "ref.locator")?,
                record_index,
                &pointer,
                event_seq,
                carrier.as_deref(),
            ))
        }
        SCHEMA_ARCHIVE_RECORD_V1 => {
            reject_unknown(map, &["record_sha256", "projection_offset"], "ref.locator")?;
            let record_sha256 = required_string(map, "record_sha256", "ref.locator")?;
            if !crate::recall::providers::archive::is_sha256(&record_sha256) {
                return Err(RecallError::invalid(
                    "ref.locator.record_sha256",
                    "invalid_sha256",
                    "record_sha256 must be 64 lowercase hex digits",
                ));
            }
            let projection_offset = required_u64(map, "projection_offset", "ref.locator")?;
            Ok(Locator::archive(&record_sha256, projection_offset))
        }
        _ => Err(RecallError::invalid(
            "ref.schema_gen",
            "unsupported_schema_gen",
            "unknown schema_gen",
        )),
    }
}

fn parse_fingerprint(value: &Value) -> Result<Fingerprint, RecallError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid_type("ref.fingerprint", "object"))?;
    reject_unknown(map, &FINGERPRINT_KEYS, "ref.fingerprint")?;
    let range_sha256 = required_string(map, "range_sha256", "ref.fingerprint")?;
    let file_sha256 = optional_string(map, "file_sha256", "ref.fingerprint")?;
    let row_sha256 = optional_string(map, "row_sha256", "ref.fingerprint")?;
    if file_sha256.is_some() && row_sha256.is_some() {
        return Err(RecallError::invalid(
            "ref.fingerprint",
            "ambiguous_fingerprint",
            "file_sha256 and row_sha256 are mutually exclusive",
        ));
    }
    Ok(Fingerprint {
        range_sha256,
        file_sha256,
        row_sha256,
    })
}

fn parse_text_range(value: &Value) -> Result<TextRange, RecallError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid_type("ref.text_range", "object"))?;
    reject_unknown(map, &TEXT_RANGE_KEYS, "ref.text_range")?;
    let line_start = required_u64(map, "line_start", "ref.text_range")?;
    let line_end = required_u64(map, "line_end", "ref.text_range")?;
    let byte_start = required_u64(map, "byte_start", "ref.text_range")?;
    let byte_end = required_u64(map, "byte_end", "ref.text_range")?;
    if byte_start > byte_end {
        return Err(RecallError::invalid(
            "ref.text_range",
            "invalid_range",
            "byte_start must not exceed byte_end",
        ));
    }
    if line_start > line_end {
        return Err(RecallError::invalid(
            "ref.text_range",
            "invalid_range",
            "line_start must not exceed line_end",
        ));
    }
    Ok(TextRange::new(byte_start, byte_end, line_start, line_end))
}

fn reject_unknown(
    map: &Map<String, Value>,
    allowed: &[&str],
    parent: &str,
) -> Result<(), RecallError> {
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        let path = if parent.is_empty() {
            key.clone()
        } else {
            format!("{parent}.{key}")
        };
        return Err(RecallError::invalid(
            &path,
            "unknown_key",
            format!("unknown key: {path}"),
        ));
    }
    Ok(())
}

fn required_string(
    map: &Map<String, Value>,
    key: &str,
    parent: &str,
) -> Result<String, RecallError> {
    match map.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(Value::String(_)) => Err(RecallError::invalid(
            &format!("{parent}.{key}"),
            "empty_value",
            "value must not be empty",
        )),
        Some(_) => Err(invalid_type(&format!("{parent}.{key}"), "string")),
        None => Err(missing(&format!("{parent}.{key}"))),
    }
}

fn optional_string(
    map: &Map<String, Value>,
    key: &str,
    parent: &str,
) -> Result<Option<String>, RecallError> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid_type(&format!("{parent}.{key}"), "string")),
    }
}

fn required_u64(map: &Map<String, Value>, key: &str, parent: &str) -> Result<u64, RecallError> {
    match map.get(key) {
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| invalid_type(&format!("{parent}.{key}"), "u64")),
        Some(_) => Err(invalid_type(&format!("{parent}.{key}"), "u64")),
        None => Err(missing(&format!("{parent}.{key}"))),
    }
}

fn optional_integer(
    value: Option<&Value>,
    path: &str,
    min: i64,
    max: i64,
    default: usize,
) -> Result<usize, RecallError> {
    match value {
        None => Ok(default),
        Some(value) => {
            let number = value
                .as_i64()
                .ok_or_else(|| invalid_type(path, "integer"))?;
            if number < min || number > max {
                return Err(RecallError::invalid(
                    path,
                    "out_of_range",
                    format!("{path} must be an integer in {min}..={max}"),
                ));
            }
            Ok(number as usize)
        }
    }
}

fn missing(path: &str) -> RecallError {
    RecallError::invalid(path, "missing_key", format!("{path} is required"))
}

fn invalid_type(path: &str, expected: &str) -> RecallError {
    RecallError::invalid(path, "invalid_type", format!("{path} must be {expected}"))
}

fn map_read_error(error: PortError) -> RecallError {
    match error {
        PortError::NotFound(_) => RecallError::new(codes::REF_LOCATOR_MISSING, error.message()),
        PortError::Locked(_) | PortError::Corrupt(_) | PortError::Io(_) => {
            RecallError::new(codes::REF_READ_FAILED, error.message())
        }
        PortError::Schema { .. } => RecallError::new(codes::REF_READ_FAILED, error.message()),
    }
}

/// Strict snapshot: keep `db`/`storage` at the same relative position inside the
/// copy. The configuration already guarantees containment.
pub(crate) fn remap_configured(
    read_root: &Path,
    config_root: &str,
    value: Option<&str>,
) -> Option<String> {
    let value = value?;
    if read_root == Path::new(config_root) {
        return Some(value.to_string());
    }
    let rel = crate::recall::port::reader::rel_from_root(config_root, Path::new(value))?;
    Some(read_root.join(rel).to_string_lossy().to_string())
}

fn safe_join(root: &Path, rel_path: &str) -> Option<PathBuf> {
    if rel_path.is_empty() {
        return None;
    }
    if rel_path.starts_with('/') || rel_path.starts_with('\\') || rel_path.contains(':') {
        return None;
    }
    let mut path = PathBuf::from(root);
    for component in rel_path.split(['/', '\\']) {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        path.push(component);
    }
    Some(path)
}

/// Root-relative, `/`-separated name of a canonicalized path, if it really sits
/// below the canonical root. Returns `None` for anything that climbs out or
/// carries a non-regular component, so callers can fail closed.
fn rel_under_root(canonical_root: &Path, resolved: &Path) -> Option<String> {
    let rel = resolved.strip_prefix(canonical_root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => {
                parts.push(part.to_string_lossy().to_string());
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Resolve and open one text carrier without leaving the configured root.
fn open_text_at_root(
    read_root: &Path,
    rel_path: &str,
) -> Result<(PathBuf, PathBuf, File, u64), RecallError> {
    let path = safe_join(read_root, rel_path).ok_or_else(|| {
        RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "outside-root: rel_path is absolute or contains traversal",
        )
    })?;
    let canonical_root = std::fs::canonicalize(read_root).map_err(|_| {
        RecallError::new(
            codes::REF_READ_FAILED,
            "configured text root cannot be resolved",
        )
    })?;

    // Refuse every link/reparse hop below the configured root, including the
    // final file. Canonical containment alone is not enough: a junction can
    // change between the metadata walk and open.
    let mut current = PathBuf::from(read_root);
    for component in rel_path.split(['/', '\\']) {
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RecallError::new(codes::REF_LOCATOR_MISSING, "file does not exist")
            } else {
                RecallError::new(codes::REF_READ_FAILED, "file metadata cannot be read")
            }
        })?;
        let mut reparse = metadata.file_type().is_symlink();
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            reparse |= metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        }
        if reparse {
            return Err(RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "reparse-point: text readback does not follow links or junctions",
            ));
        }
    }

    let resolved = std::fs::canonicalize(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RecallError::new(codes::REF_LOCATOR_MISSING, "file does not exist")
        } else {
            RecallError::new(codes::REF_READ_FAILED, "file path cannot be resolved")
        }
    })?;
    if !resolved.starts_with(&canonical_root) {
        return Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "outside-root: resolved text path escapes the configured root",
        ));
    }
    let file = File::open(&resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RecallError::new(codes::REF_LOCATOR_MISSING, "file does not exist")
        } else {
            RecallError::new(codes::REF_READ_FAILED, "open failed")
        }
    })?;

    // On Windows, validate the path of the actual handle, not just the pathname
    // used for open. Other targets retain a residual race between resolution
    // and open; replacing an ancestor concurrently can still redirect the open.
    #[cfg(windows)]
    {
        let opened = final_path_of_open_file(&file).map_err(|_| {
            RecallError::new(
                codes::REF_READ_FAILED,
                "opened file handle path cannot be resolved",
            )
        })?;
        if !opened.starts_with(&canonical_root) {
            return Err(RecallError::new(
                codes::REF_LOCATOR_MISSING,
                "outside-root: opened file handle escapes the configured root",
            ));
        }
    }
    let file_len = file
        .metadata()
        .map_err(|_| RecallError::new(codes::REF_READ_FAILED, "file metadata cannot be read"))?
        .len();
    Ok((resolved, canonical_root, file, file_len))
}

#[cfg(windows)]
fn final_path_of_open_file(file: &File) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFinalPathNameByHandleW(
            file: *mut core::ffi::c_void,
            path: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
    }

    let handle = file.as_raw_handle() as *mut core::ffi::c_void;
    // FILE_NAME_NORMALIZED | VOLUME_NAME_DOS.
    let flags = 0;
    let required = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, flags) };
    if required == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buffer = vec![0u16; required as usize + 1];
    let written = unsafe {
        GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, flags)
    };
    if written == 0 || written as usize >= buffer.len() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(std::ffi::OsString::from_wide(&buffer[..written as usize]).into())
}

fn read_range(file: &mut File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let len = end.saturating_sub(start);
    if len > MAX_REF_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "range above read budget",
        ));
    }
    let mut buffer = vec![0u8; len as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

/// Consume `newlines` newlines scanning backwards from `position`; returns the
/// byte offset just after the last consumed newline, or 0.
fn find_line_start(file: &mut File, position: u64, newlines: usize) -> std::io::Result<u64> {
    let mut remaining = newlines;
    let mut cursor = position;
    let mut buffer = [0u8; 4096];
    while cursor > 0 && remaining > 0 {
        let take = cursor.min(4096) as usize;
        let start = cursor - take as u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..take])?;
        let mut index = take;
        while index > 0 {
            index -= 1;
            if buffer[index] == b'\n' {
                remaining -= 1;
                if remaining == 0 {
                    return Ok(start + index as u64 + 1);
                }
            }
        }
        cursor = start;
    }
    Ok(0)
}

/// Consume `lines + 1` newlines scanning forward from `position`; returns the
/// offset just after the last consumed newline (or EOF).
fn find_forward_end(
    file: &mut File,
    position: u64,
    lines: usize,
    file_len: u64,
) -> std::io::Result<u64> {
    let mut remaining = lines + 1;
    let mut cursor = position;
    let mut buffer = [0u8; 4096];
    while cursor < file_len {
        let take = ((file_len - cursor) as usize).min(4096);
        file.seek(SeekFrom::Start(cursor))?;
        file.read_exact(&mut buffer[..take])?;
        for index in 0..take {
            if buffer[index] == b'\n' {
                remaining -= 1;
                if remaining == 0 {
                    return Ok(cursor + index as u64 + 1);
                }
            }
        }
        cursor += take as u64;
    }
    Ok(file_len)
}

/// Largest UTF-8 boundary (at least one character) within `limit` bytes.
fn utf8_floor_bytes(bytes: &[u8], limit: usize) -> usize {
    if bytes.len() <= limit {
        return bytes.len();
    }
    let mut cut = limit;
    while cut > 0 && std::str::from_utf8(&bytes[..cut]).is_err() {
        cut -= 1;
    }
    if cut == 0 && !bytes.is_empty() {
        let mut width = 1usize;
        let first = bytes[0];
        if first & 0b1111_1000 == 0b1111_0000 {
            width = 4;
        } else if first & 0b1111_0000 == 0b1110_0000 {
            width = 3;
        } else if first & 0b1110_0000 == 0b1100_0000 {
            width = 2;
        }
        width.min(bytes.len())
    } else {
        cut
    }
}
