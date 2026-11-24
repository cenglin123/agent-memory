//! DSH (DeepSeek Harness) session logs (`dsh-jsonl`).
//!
//! Layout: `<root>/<project-slug>/<session-dir>/<generation file>`.
//!
//! * `<project-slug>` is a lossy encoding of the session `cwd`; it is never
//!   decoded. The authoritative cwd is the header line.
//! * `<session-dir>` is `session-…` for a top-level session and a bare UUID for
//!   a subagent (worker).
//! * One directory can hold several immutable generations:
//!   `session.jsonl.zstd` (logical format v0/v1), `session.v3.jsonl.zstd` and
//!   `session.v4.jsonl.zstd`. Only the highest generation is read. A generation
//!   this build does not know (for example `session.v5.jsonl.zstd`) makes the
//!   whole directory `SCHEMA_UNSUPPORTED`; the reader never guesses.
//!
//! Physical shape: one per-session append-only logical JSONL stream stored as a
//! sequence of checksummed Zstandard frames. Reading is frame by frame: a torn
//! last frame stops the stream at the last complete line and is disclosed as
//! `partial_tail`, while an interior corrupt frame is `RECORD_SKIPPED`. A
//! plaintext `.jsonl` carrier (DSH configured with `compression: none`) is read
//! as one region with the same line rules.
//!
//! Projection (one definition shared by search, the live index, `memory_read`
//! and `memory_recent`, so a tail ref and a search-hit ref are byte-identical):
//! user/assistant text blocks, `tool/call` inputs and `tool/result` outputs.
//! `reasoning`, `system/message`, `agent/inbox/spliced`, chunks and every other
//! envelope are not projected.

use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{
    codes, status, ROLE_ASSISTANT, ROLE_TOOL, ROLE_USER, SCHEMA_DSH_JSONL, SCORE_KIND_NONE,
    UNIT_KIND_TEXT, UNIT_KIND_TOOL_INPUT, UNIT_KIND_TOOL_OUTPUT,
};
use crate::recall::hash::{root_sha256, sha256_hex, TextRange};
use crate::recall::port::reader::rel_from_root;
use crate::recall::port::Health;
use crate::recall::providers::{rank_hits, SourceHit, SourceOutcome};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::text::{fold_ascii, lexical_match, snippet};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Logical (decompressed) byte ceiling for one session log. It is shared by
/// search, index, readback and the recent tail so one session can never make a
/// request unbounded.
pub const DSH_SCAN_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
/// Physical (compressed) file ceiling: a file above it is refused instead of
/// being read into memory.
pub const DSH_PHYSICAL_LIMIT_BYTES: u64 = 256 * 1024 * 1024;
/// Highest logical session format generation this build understands.
pub const DSH_MAX_GENERATION: u32 = 4;
/// Streaming decode chunk. A frame is never materialized whole: it is fed to
/// the accumulator in chunks so [`DSH_SCAN_LIMIT_BYTES`] applies while decoding
/// (a high-ratio frame cannot allocate more than the ceiling plus one chunk).
const STREAM_CHUNK_BYTES: usize = 256 * 1024;
/// The header reader stops after this many decoded bytes (the header is line 0).
const HEADER_DECODE_LIMIT_BYTES: usize = 1024 * 1024;

/// One immutable generation file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DshGeneration {
    /// `session.jsonl.zstd`: logical format v0 or v1 (header decides).
    Legacy,
    V3,
    V4,
}

impl DshGeneration {
    pub fn version(self) -> u32 {
        match self {
            DshGeneration::Legacy => 1,
            DshGeneration::V3 => 3,
            DshGeneration::V4 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DshFilename {
    Supported(DshGeneration),
    /// A canonical `session.vN` name this build does not read.
    Unsupported(u32),
}

/// Parse one canonical generation filename. `.jsonl.zstd` (the default DSH
/// storage) and plaintext `.jsonl` are both accepted; temporary, uppercase,
/// leading-zero and non-`session` names are not generation files.
///
/// Canonical `session.v1.jsonl(.zstd)` / `session.v2.jsonl(.zstd)` parse as
/// [`DshFilename::Unsupported`]: their header shape is not implemented, so the
/// directory is refused (`SCHEMA_UNSUPPORTED`) instead of being read with the
/// v0/v1 rules.
pub fn parse_log_filename(name: &str) -> Option<DshFilename> {
    let stem = name
        .strip_suffix(".zstd")
        .unwrap_or(name)
        .strip_suffix(".jsonl")?;
    if stem == "session" {
        return Some(DshFilename::Supported(DshGeneration::Legacy));
    }
    let rest = stem.strip_prefix("session.v")?;
    if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let version: u32 = rest.parse().ok()?;
    if rest != version.to_string() {
        return None;
    }
    Some(match version {
        3 => DshFilename::Supported(DshGeneration::V3),
        4 => DshFilename::Supported(DshGeneration::V4),
        other => DshFilename::Unsupported(other),
    })
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DshSessionFile {
    /// `<project-slug>/<session-dir>` (forward slashes).
    pub rel_dir: String,
    /// `<project-slug>/<session-dir>/<carrier>` (forward slashes).
    pub rel_path: String,
    pub path: PathBuf,
    pub generation: DshGeneration,
    pub dir_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct DshInventory {
    pub sessions: Vec<DshSessionFile>,
    /// Session directories whose highest generation is unknown.
    pub unsupported: Vec<String>,
}

fn is_dir_entry(entry: &std::fs::DirEntry) -> bool {
    entry
        .file_type()
        .map(|kind| kind.is_dir() && !kind.is_symlink())
        .unwrap_or(false)
}

/// Enumerate every `<root>/<project>/<session-dir>` and select its highest
/// readable generation. Symlinks are never followed.
pub fn inventory(root: &Path) -> Result<DshInventory, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("dsh root cannot be enumerated: {error}"))?;
    let mut projects: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("dsh root cannot be enumerated: {error}"))?;
        if is_dir_entry(&entry) {
            projects.push(entry.path());
        }
    }
    projects.sort();
    let mut sessions = Vec::new();
    let mut unsupported = Vec::new();
    for project in projects {
        let Some(project_name) = project.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(children) = std::fs::read_dir(&project) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for child in children.flatten() {
            if is_dir_entry(&child) {
                dirs.push(child.path());
            }
        }
        dirs.sort();
        for dir in dirs {
            let Some(dir_name) = dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut best: Option<(DshGeneration, String)> = None;
            let mut future: Option<u32> = None;
            for file in files.flatten() {
                let Ok(file_type) = file.file_type() else {
                    continue;
                };
                if file_type.is_symlink() || !file_type.is_file() {
                    continue;
                }
                let name = file.file_name().to_string_lossy().to_string();
                match parse_log_filename(&name) {
                    Some(DshFilename::Supported(generation)) => {
                        if best
                            .as_ref()
                            .map(|(current, _)| generation > *current)
                            .unwrap_or(true)
                        {
                            best = Some((generation, name));
                        }
                    }
                    Some(DshFilename::Unsupported(version)) => {
                        future = Some(future.unwrap_or(0).max(version));
                    }
                    None => {}
                }
            }
            let rel_dir = format!("{project_name}/{dir_name}");
            let supported_version = best.as_ref().map(|(g, _)| g.version()).unwrap_or(0);
            if let Some(version) = future {
                if version > supported_version {
                    unsupported.push(rel_dir);
                    continue;
                }
            }
            let Some((generation, name)) = best else {
                continue;
            };
            sessions.push(DshSessionFile {
                rel_dir: rel_dir.clone(),
                rel_path: format!("{rel_dir}/{name}"),
                path: dir.join(&name),
                generation,
                dir_name: dir_name.to_string(),
            });
        }
    }
    sessions.sort_by(|left, right| left.rel_dir.cmp(&right.rel_dir));
    unsupported.sort();
    Ok(DshInventory {
        sessions,
        unsupported,
    })
}

// ---------------------------------------------------------------------------
// Frame-level reading
// ---------------------------------------------------------------------------

/// How much of the physical stream is readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DshReadState {
    /// Every frame decoded and ended on a complete line.
    Complete,
    /// The last frame is torn or fails its checksum (the documented crashed
    /// append). Complete lines before it are usable.
    PartialTail,
    /// An interior frame is corrupt, or the logical cap was hit: the session is
    /// counted `RECORD_SKIPPED`.
    Corrupt,
}

pub struct DshLog {
    pub rel_path: String,
    pub physical_len: u64,
    pub frames: usize,
    pub state: DshReadState,
    /// The full-read decompressed stream exceeded [`DSH_SCAN_LIMIT_BYTES`].
    pub truncated: bool,
    /// A tail read dropped older complete lines to stay inside the ceiling.
    pub window_truncated: bool,
    /// Logical line index of the first retained line.
    first_index: u64,
    logical: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct DshLine<'a> {
    /// 0-based logical line index (the header is line 0).
    pub index: u64,
    /// Line bytes without the terminating `\n` (a trailing `\r` is stripped).
    pub bytes: &'a [u8],
    /// Byte offset of this line's first byte inside the logical stream. The
    /// archive collector uses it as the record's logical source offset.
    pub start: u64,
    /// Byte offset just past the terminating `\n` (the start of the next line).
    pub end: u64,
}

pub struct DshLines<'a> {
    bytes: &'a [u8],
    position: usize,
    index: u64,
    done: bool,
}

impl<'a> Iterator for DshLines<'a> {
    type Item = DshLine<'a>;

    fn next(&mut self) -> Option<DshLine<'a>> {
        if self.done {
            return None;
        }
        let rest = &self.bytes[self.position..];
        let Some(offset) = rest.iter().position(|byte| *byte == b'\n') else {
            // A trailing fragment without a newline is a torn append, not a
            // line: it is never produced.
            self.done = true;
            return None;
        };
        let mut end = offset;
        while end > 0 && rest[end - 1] == b'\r' {
            end -= 1;
        }
        let line = DshLine {
            index: self.index,
            bytes: &rest[..end],
            start: self.position as u64,
            end: (self.position + offset + 1) as u64,
        };
        self.position += offset + 1;
        self.index += 1;
        Some(line)
    }
}

impl DshLog {
    pub fn lines(&self) -> DshLines<'_> {
        DshLines {
            bytes: &self.logical,
            position: 0,
            index: self.first_index,
            done: false,
        }
    }

    /// Complete lines only (a torn fragment is not counted).
    pub fn line_count(&self) -> u64 {
        self.first_index + self.logical.iter().filter(|byte| **byte == b'\n').count() as u64
    }

    pub fn first_line(&self) -> Option<&[u8]> {
        self.lines().next().map(|line| line.bytes)
    }

    /// The retained logical bytes (the archive collector commits exact slices
    /// of this stream).
    pub fn logical_bytes(&self) -> &[u8] {
        &self.logical
    }
}

/// Build a [`DshLog`] from already-decoded logical bytes: stage-2 S5 rebuilds
/// committed evidence into memory, and the archive-fallback readback must run
/// the same line/projection rules as a live carrier instead of a second scanner.
pub fn log_from_logical(rel_path: &str, logical: Vec<u8>) -> DshLog {
    DshLog {
        rel_path: rel_path.to_string(),
        physical_len: logical.len() as u64,
        frames: 0,
        state: DshReadState::Complete,
        truncated: false,
        window_truncated: false,
        first_index: 0,
        logical,
    }
}

fn zstd_magic_at(bytes: &[u8]) -> Option<usize> {
    const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    bytes.windows(4).position(|window| window == MAGIC)
}

/// Split the physical stream into complete frames. Bytes after the last
/// complete frame are classified: a torn/unreadable tail is `partial_tail`,
/// while a damaged frame followed by a readable frame is `corrupt`.
fn frame_spans(bytes: &[u8]) -> (Vec<(usize, usize)>, DshReadState, u64) {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut state = DshReadState::Complete;
    let mut corrupt = 0u64;
    let mut position = 0usize;
    while position < bytes.len() {
        match zstd::zstd_safe::find_frame_compressed_size(&bytes[position..]) {
            Ok(size) if size > 0 => {
                if position + size > bytes.len() {
                    // The declared frame runs past EOF: the tail is torn.
                    state = DshReadState::PartialTail;
                    break;
                }
                spans.push((position, position + size));
                position += size;
            }
            _ => {
                // Damaged header/body. Try to resynchronize on the next frame
                // magic; a later readable frame proves the damage is interior.
                if let Some(offset) = zstd_magic_at(&bytes[position.saturating_add(1)..]) {
                    let candidate = position + 1 + offset;
                    if let Ok(size) =
                        zstd::zstd_safe::find_frame_compressed_size(&bytes[candidate..])
                    {
                        if size > 0 && candidate + size <= bytes.len() {
                            corrupt += 1;
                            state = DshReadState::Corrupt;
                            position = candidate;
                            continue;
                        }
                    }
                }
                state = DshReadState::PartialTail;
                break;
            }
        }
    }
    (spans, state, corrupt)
}

/// Decode a carrier. `keep_tail` retains only the last
/// [`DSH_SCAN_LIMIT_BYTES`] of the logical stream (counting dropped lines so
/// absolute indices stay exact); otherwise the head is kept and decoding stops
/// at the ceiling (`truncated`), so a frame is never fully materialized.
fn decode_stream(path: &Path, rel_path: &str, keep_tail: bool) -> Result<DshLog, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("{rel_path}: {error}"))?;
    if metadata.len() > DSH_PHYSICAL_LIMIT_BYTES {
        return Err(format!(
            "{rel_path}: carrier exceeds the physical read ceiling ({} bytes)",
            metadata.len()
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("{rel_path}: {error}"))?;
    let compressed = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.ends_with(".zstd"))
        .unwrap_or(false);
    let mut stream = Accumulator::new(keep_tail);

    let (frames, mut state) = if compressed {
        let (spans, mut state, mut corrupt_frames) = frame_spans(&bytes);
        for (position, (start, end)) in spans.iter().enumerate() {
            // Streaming decode: the frame's logical bytes are consumed in
            // chunks, and the head cap stops reading as soon as it is reached.
            // A frame that fails is rolled back for a head read, so a corrupt
            // frame contributes no lines (its decoded prefix is discarded).
            let mark = stream.mark();
            let decoded = zstd::stream::Decoder::new(&bytes[*start..*end])
                .map_err(|error| error.to_string())
                .and_then(|mut decoder| {
                    stream
                        .absorb_reader(&mut decoder)
                        .map_err(|error| error.to_string())
                });
            if decoded.is_err() {
                stream.rollback(mark);
                corrupt_frames += 1;
                state = if position + 1 == spans.len() && state == DshReadState::Complete {
                    DshReadState::PartialTail
                } else {
                    DshReadState::Corrupt
                };
                break;
            }
            if stream.truncated {
                break;
            }
        }
        if corrupt_frames > 0 && state == DshReadState::Complete {
            state = DshReadState::PartialTail;
        }
        (spans.len(), state)
    } else {
        // `compression = none`: one logical region. A trailing fragment without
        // a newline is the same torn append a zstd frame tail represents.
        stream.absorb(&bytes);
        let torn = !bytes.is_empty() && !bytes.ends_with(b"\n");
        (
            usize::from(!bytes.is_empty()),
            if torn {
                DshReadState::PartialTail
            } else {
                DshReadState::Complete
            },
        )
    };
    if stream.truncated {
        state = DshReadState::Corrupt;
    }
    Ok(DshLog {
        rel_path: rel_path.to_string(),
        physical_len: metadata.len(),
        frames,
        state,
        truncated: stream.truncated,
        window_truncated: stream.window_truncated,
        first_index: stream.first_index(),
        logical: stream.logical,
    })
}

/// Decompressed-stream accumulator. A full read keeps the head up to the
/// ceiling; a tail read keeps the last ceiling-sized window and counts the
/// dropped complete lines so indices stay absolute.
struct Accumulator {
    keep_tail: bool,
    cap: usize,
    logical: Vec<u8>,
    truncated: bool,
    window_truncated: bool,
    dropped_lines: u64,
    dropped_ends_with_newline: bool,
}

/// Rollback point for a failed frame (head reads only: a tail window may have
/// already dropped older bytes and cannot be rewound cheaply).
#[derive(Debug, Clone, Copy)]
struct Mark {
    len: usize,
    truncated: bool,
}

impl Accumulator {
    fn new(keep_tail: bool) -> Self {
        Accumulator {
            keep_tail,
            cap: DSH_SCAN_LIMIT_BYTES as usize,
            logical: Vec::new(),
            truncated: false,
            window_truncated: false,
            dropped_lines: 0,
            dropped_ends_with_newline: true,
        }
    }

    fn absorb(&mut self, chunk: &[u8]) {
        if self.keep_tail {
            self.logical.extend_from_slice(chunk);
            if self.logical.len() > self.cap {
                let drop = self.logical.len() - self.cap;
                self.dropped_lines += self.logical[..drop]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count() as u64;
                self.dropped_ends_with_newline = self.logical[drop - 1] == b'\n';
                self.logical.drain(..drop);
                self.window_truncated = true;
            }
        } else if self.logical.len() + chunk.len() > self.cap {
            let room = self.cap.saturating_sub(self.logical.len());
            self.logical
                .extend_from_slice(&chunk[..room.min(chunk.len())]);
            self.truncated = true;
        } else {
            self.logical.extend_from_slice(chunk);
        }
    }

    /// Consume a decoder in bounded chunks. A head read stops at the ceiling
    /// (the rest of the frame is never decoded); a tail read keeps draining so
    /// the window stays at the last `cap` bytes.
    fn absorb_reader(&mut self, reader: &mut impl Read) -> std::io::Result<()> {
        let mut chunk = vec![0u8; STREAM_CHUNK_BYTES];
        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                return Ok(());
            }
            self.absorb(&chunk[..read]);
            if !self.keep_tail && self.truncated {
                return Ok(());
            }
        }
    }

    fn mark(&self) -> Mark {
        Mark {
            len: self.logical.len(),
            truncated: self.truncated,
        }
    }

    /// Discard what a failed frame appended. A tail read keeps its window (the
    /// reader already reports the carrier as corrupt/partial); a head read
    /// restores the exact pre-frame prefix.
    fn rollback(&mut self, mark: Mark) {
        if self.keep_tail {
            return;
        }
        self.logical.truncate(mark.len);
        self.truncated = mark.truncated;
    }

    fn first_index(&mut self) -> u64 {
        if self.window_truncated && !self.dropped_ends_with_newline {
            // The retained window starts inside a line; that line's prefix is
            // gone, so it is not a readable line.
            if let Some(position) = self.logical.iter().position(|byte| *byte == b'\n') {
                self.logical.drain(..=position);
                self.dropped_lines += 1;
            }
        }
        self.dropped_lines
    }
}

/// Read one log file from the start. The whole physical file is bounded by
/// [`DSH_PHYSICAL_LIMIT_BYTES`]; the decompressed stream by
/// [`DSH_SCAN_LIMIT_BYTES`].
pub fn open_log(path: &Path, rel_path: &str) -> Result<DshLog, String> {
    decode_stream(path, rel_path, false)
}

/// Read one log file keeping only its logical tail (the `memory_recent` path).
/// Line indices remain absolute: dropped lines are counted, not renumbered.
pub fn open_tail(path: &Path, rel_path: &str) -> Result<DshLog, String> {
    decode_stream(path, rel_path, true)
}

// ---------------------------------------------------------------------------
// Header, metadata and projection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DshHeader {
    pub version: u32,
    pub session_id: String,
    pub cwd: Option<String>,
    pub parent_session: Option<String>,
    pub origin: Option<String>,
}

pub fn parse_header(line: &[u8]) -> Option<DshHeader> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let session_id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    Some(DshHeader {
        version: value
            .get("version")
            .and_then(Value::as_u64)
            .unwrap_or_default() as u32,
        session_id,
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_string),
        parent_session: value
            .get("parentSession")
            .and_then(Value::as_str)
            .filter(|parent| !parent.is_empty())
            .map(str::to_string),
        origin: value
            .get("origin")
            .and_then(Value::as_str)
            .filter(|origin| !origin.is_empty())
            .map(str::to_string),
    })
}

/// Session-level facts shared by every unit of one session.
#[derive(Debug, Clone)]
pub struct DshSessionMeta {
    pub session_id: String,
    pub session_kind: &'static str,
    pub title: String,
    pub cwd: Option<String>,
    /// Newest `user/message` / `assistant/message` time.
    pub conversation_ms: Option<i64>,
    /// Newest event time of any kind.
    pub any_ms: Option<i64>,
}

pub fn event_type(value: &Value) -> &str {
    value.get("type").and_then(Value::as_str).unwrap_or("")
}

pub fn event_seq(value: &Value) -> Option<u64> {
    value.get("seq").and_then(Value::as_u64)
}

pub fn event_time(value: &Value) -> Option<i64> {
    value.get("time").and_then(Value::as_i64)
}

fn is_conversation_event(value: &Value) -> bool {
    matches!(event_type(value), "user/message" | "assistant/message")
}

/// Decode just enough of the carrier to read the header line. This never reads
/// or decompresses the whole session: a compressed carrier contributes only its
/// first frame, decoded until the first newline or the header size bound.
pub fn read_header(path: &Path, rel_path: &str) -> Result<DshHeader, String> {
    let compressed = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.ends_with(".zstd"))
        .unwrap_or(false);
    let mut buffer: Vec<u8> = Vec::new();
    if compressed {
        let frame = read_first_frame(path, rel_path)?;
        let mut decoder = zstd::stream::Decoder::new(frame.as_slice())
            .map_err(|_| format!("{rel_path}: header frame is unreadable"))?;
        read_header_bytes(&mut decoder, &mut buffer)
            .map_err(|error| format!("{rel_path}: header frame is unreadable: {error}"))?;
    } else {
        let file = std::fs::File::open(path).map_err(|error| format!("{rel_path}: {error}"))?;
        let mut limited = file.take(HEADER_DECODE_LIMIT_BYTES as u64);
        limited
            .read_to_end(&mut buffer)
            .map_err(|error| format!("{rel_path}: {error}"))?;
    }
    buffer.truncate(HEADER_DECODE_LIMIT_BYTES);
    let end = buffer
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| format!("{rel_path}: header line is missing"))?;
    let mut line = &buffer[..end];
    while line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    parse_header(line).ok_or_else(|| format!("{rel_path}: header line is not a session header"))
}

/// Read bytes until the first newline or the header bound, whichever comes
/// first. The decoder is dropped right after, so a high-ratio first frame can
/// never be materialized in full.
fn read_header_bytes(reader: &mut impl Read, buffer: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    while buffer.len() < HEADER_DECODE_LIMIT_BYTES {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.contains(&b'\n') {
            break;
        }
    }
    Ok(())
}

/// The first complete Zstandard frame of a carrier, read from disk without
/// touching the rest of the file (a large or sparse tail costs nothing).
fn read_first_frame(path: &Path, rel_path: &str) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::open(path).map_err(|error| format!("{rel_path}: {error}"))?;
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if !buffer.is_empty() {
            if let Ok(size) = zstd::zstd_safe::find_frame_compressed_size(&buffer) {
                if size > 0 {
                    if size as u64 > DSH_PHYSICAL_LIMIT_BYTES {
                        return Err(format!(
                            "{rel_path}: header frame exceeds the physical read ceiling ({size} bytes)"
                        ));
                    }
                    if size > buffer.len() {
                        let start = buffer.len();
                        buffer.resize(size, 0);
                        file.read_exact(&mut buffer[start..])
                            .map_err(|error| format!("{rel_path}: {error}"))?;
                    }
                    buffer.truncate(size);
                    return Ok(buffer);
                }
            }
        }
        let read = file
            .read(&mut chunk)
            .map_err(|error| format!("{rel_path}: {error}"))?;
        if read == 0 {
            return Err(format!("{rel_path}: header frame is missing"));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() as u64 > DSH_PHYSICAL_LIMIT_BYTES {
            return Err(format!(
                "{rel_path}: header frame exceeds the physical read ceiling"
            ));
        }
    }
}

/// Derive the session facts from the header plus every event line. Returns
/// `None` for a header this build does not understand (`SCHEMA_UNSUPPORTED`).
pub fn scan_meta(log: &DshLog, dir_name: &str) -> Option<DshSessionMeta> {
    let header = parse_header(log.first_line()?)?;
    scan_meta_with_header(&header, log, dir_name)
}

/// [`scan_meta`] for a tail log whose first retained line is not the header.
pub fn scan_meta_with_header(
    header: &DshHeader,
    log: &DshLog,
    dir_name: &str,
) -> Option<DshSessionMeta> {
    if header.version > DSH_MAX_GENERATION {
        return None;
    }
    let mut title: Option<String> = None;
    let mut descriptor: Option<String> = None;
    let mut conversation_ms: Option<i64> = None;
    let mut any_ms: Option<i64> = None;
    for line in log.lines() {
        let Ok(value) = serde_json::from_slice::<Value>(line.bytes) else {
            continue;
        };
        if let Some(time) = event_time(&value) {
            any_ms = Some(any_ms.unwrap_or(time).max(time));
            if is_conversation_event(&value) {
                conversation_ms = Some(conversation_ms.unwrap_or(time).max(time));
            }
        }
        match event_type(&value) {
            "session/title" => {
                if let Some(value) = value
                    .get("data")
                    .and_then(|data| data.get("title"))
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    title = Some(value.to_string());
                }
            }
            "subagent/descriptor" => {
                if let Some(value) = value
                    .get("data")
                    .and_then(|data| data.get("label"))
                    .and_then(Value::as_str)
                    .filter(|label| !label.trim().is_empty())
                {
                    descriptor = Some(value.to_string());
                }
            }
            _ => {}
        }
    }
    let session_kind = crate::recall::fulltext::dsh_session_kind(
        header.origin.as_deref(),
        header.parent_session.as_deref(),
        dir_name,
    );
    let title = match title {
        Some(title) => title,
        None if session_kind == crate::recall::contract::SESSION_KIND_WORKER => {
            descriptor.unwrap_or_default()
        }
        None => String::new(),
    };
    Some(DshSessionMeta {
        session_id: header.session_id.clone(),
        session_kind,
        title,
        cwd: header.cwd.clone(),
        conversation_ms,
        any_ms,
    })
}

/// One searchable projection of one DSH event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshUnit {
    pub pointer: String,
    pub text: String,
    pub role: &'static str,
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DshEventOutcome {
    Units(Vec<DshUnit>),
    /// A legal envelope with no searchable text under the DSH rules.
    Empty,
    /// An envelope type this source does not project; counted by literal.
    Ignored { type_literal: String },
}

fn text_of(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|text| !text.trim().is_empty())
}

fn text_blocks(content: Option<&Value>, prefix: &str, role: &'static str) -> Vec<DshUnit> {
    let mut units = Vec::new();
    match content {
        None => {}
        Some(Value::String(text)) => {
            if !text.trim().is_empty() {
                units.push(DshUnit {
                    pointer: prefix.to_string(),
                    text: text.clone(),
                    role,
                    kind: UNIT_KIND_TEXT,
                });
            }
        }
        Some(Value::Array(blocks)) => {
            for (index, block) in blocks.iter().enumerate() {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let Some(text) = text_of(block.get("text")) else {
                    continue;
                };
                units.push(DshUnit {
                    pointer: format!("{prefix}/{index}"),
                    text: text.to_string(),
                    role,
                    kind: UNIT_KIND_TEXT,
                });
            }
        }
        Some(_) => {}
    }
    units
}

fn user_units(data: &Value) -> DshEventOutcome {
    // A `source.kind` that is present and is not `user` marks a client-injected
    // or synthetic message; it is not user-authored text.
    let message = data.get("message");
    let source_kind = data
        .get("source")
        .or_else(|| message.and_then(|message| message.get("source")))
        .and_then(|source| source.get("kind"))
        .and_then(Value::as_str);
    if let Some(kind) = source_kind {
        if kind != "user" {
            return DshEventOutcome::Empty;
        }
    }
    let (content, prefix) = match data.get("content") {
        Some(content) => (Some(content), "/data/content"),
        None => (
            message.and_then(|message| message.get("content")),
            "/data/message/content",
        ),
    };
    let units = text_blocks(content, prefix, ROLE_USER);
    if units.is_empty() {
        DshEventOutcome::Empty
    } else {
        DshEventOutcome::Units(units)
    }
}

fn assistant_units(data: &Value) -> DshEventOutcome {
    let message = data.get("message").unwrap_or(&Value::Null);
    if let Some(role) = message.get("role").and_then(Value::as_str) {
        if role != "assistant" {
            return DshEventOutcome::Empty;
        }
    }
    let units = text_blocks(
        message.get("content"),
        "/data/message/content",
        ROLE_ASSISTANT,
    );
    if units.is_empty() {
        DshEventOutcome::Empty
    } else {
        DshEventOutcome::Units(units)
    }
}

fn tool_call_units(data: &Value) -> DshEventOutcome {
    let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
    let arguments = data
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.trim().is_empty() && arguments.trim().is_empty() {
        return DshEventOutcome::Empty;
    }
    let text = if arguments.trim().is_empty() {
        name.to_string()
    } else {
        format!("{name}\n{arguments}")
    };
    DshEventOutcome::Units(vec![DshUnit {
        pointer: "/data/arguments".to_string(),
        text,
        role: ROLE_TOOL,
        kind: UNIT_KIND_TOOL_INPUT,
    }])
}

/// Text blocks of a v4 tool result (`data.message.content[]`), or of the v3
/// wrapper (`data.message.content[0]` = `{type: "tool-result", content: […]}`).
fn tool_result_units(data: &Value) -> DshEventOutcome {
    let message = data.get("message").unwrap_or(&Value::Null);
    let content = message.get("content");
    let mut units = Vec::new();
    let wrapper = content
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool-result"));
    match wrapper {
        Some(wrapper) => {
            let inner = wrapper.get("content").or_else(|| wrapper.get("output"));
            match inner {
                Some(Value::Array(blocks)) => {
                    for (index, block) in blocks.iter().enumerate() {
                        if block.get("type").and_then(Value::as_str) != Some("text") {
                            continue;
                        }
                        let Some(text) = text_of(block.get("text")) else {
                            continue;
                        };
                        units.push(DshUnit {
                            pointer: format!("/data/message/content/0/content/{index}"),
                            text: text.to_string(),
                            role: ROLE_TOOL,
                            kind: UNIT_KIND_TOOL_OUTPUT,
                        });
                    }
                }
                Some(Value::String(text)) if !text.trim().is_empty() => {
                    units.push(DshUnit {
                        pointer: "/data/message/content/0/content".to_string(),
                        text: text.clone(),
                        role: ROLE_TOOL,
                        kind: UNIT_KIND_TOOL_OUTPUT,
                    });
                }
                _ => {}
            }
        }
        None => {
            for (index, block) in content
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let Some(text) = text_of(block.get("text")) else {
                    continue;
                };
                units.push(DshUnit {
                    pointer: format!("/data/message/content/{index}"),
                    text: text.to_string(),
                    role: ROLE_TOOL,
                    kind: UNIT_KIND_TOOL_OUTPUT,
                });
            }
        }
    }
    if units.is_empty() {
        DshEventOutcome::Empty
    } else {
        DshEventOutcome::Units(units)
    }
}

/// The single projection definition for one event envelope.
pub fn project_event(value: &Value) -> DshEventOutcome {
    let event_type = event_type(value);
    if event_type.is_empty() {
        return DshEventOutcome::Empty;
    }
    let data = value.get("data").unwrap_or(&Value::Null);
    match event_type {
        "user/message" => user_units(data),
        "assistant/message" => assistant_units(data),
        "tool/call" => tool_call_units(data),
        "tool/result" => tool_result_units(data),
        other => DshEventOutcome::Ignored {
            type_literal: other.to_string(),
        },
    }
}

/// True when a DSH session is a worker (never returned by the lexical path).
pub fn is_worker(meta: &DshSessionMeta) -> bool {
    meta.session_kind == crate::recall::contract::SESSION_KIND_WORKER
}

// ---------------------------------------------------------------------------
// Lexical search (the no-index fallback, same capability as other live sources)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Bytes,
    Time,
}

#[derive(Debug)]
struct PendingHit {
    line_index: u64,
    event_seq: Option<u64>,
    pointer: String,
    text: String,
    range_sha256: String,
    row_sha256: String,
    time: i64,
}

pub fn search_at(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    session_filter: Option<&str>,
    read_root: &Path,
) -> SourceOutcome {
    let started = Instant::now();
    let inventory = match inventory(read_root) {
        Ok(inventory) => inventory,
        Err(message) => {
            return SourceOutcome {
                source_id: source.source_id.clone(),
                status: status::UNAVAILABLE,
                code: Some(codes::SOURCE_ROOT_INVALID),
                detail: Some(serde_json::json!({ "reason": "root_unreadable" })),
                hits: Vec::new(),
                warnings: Vec::new(),
                error_message: Some(message),
                side_effects: Vec::new(),
                violations: Vec::new(),
            }
        }
    };
    let total_sessions = inventory.sessions.len() as u64;
    let unsupported_sessions = inventory.unsupported.len() as u64;
    let mut health = Health::default();
    let mut scanned_sessions = 0u64;
    let mut partial_tail = 0u64;
    let mut corrupt_sessions = 0u64;
    let mut ignored_types: BTreeMap<String, u64> = BTreeMap::new();
    let mut hits: Vec<SourceHit> = Vec::new();
    let mut stop: Option<StopReason> = None;

    'sessions: for session in &inventory.sessions {
        if health.bytes >= budget.max_bytes_per_source {
            stop = Some(StopReason::Bytes);
            break;
        }
        if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
            stop = Some(StopReason::Time);
            break;
        }
        let log = match open_log(&session.path, &session.rel_path) {
            Ok(log) => log,
            Err(_) => {
                health.error();
                continue;
            }
        };
        health.consume(log.physical_len);
        if log.state == DshReadState::PartialTail {
            partial_tail += 1;
        }
        if log.state == DshReadState::Corrupt {
            corrupt_sessions += 1;
            health.skip(0);
        }
        let Some(meta) = scan_meta(&log, &session.dir_name) else {
            health.skip_without_bytes();
            continue;
        };
        if is_worker(&meta) {
            continue;
        }
        if let Some(filter) = session_filter {
            if meta.session_id != filter && session.dir_name != filter {
                continue;
            }
        }
        scanned_sessions += 1;
        let mut pending: Vec<PendingHit> = Vec::new();
        for line in log.lines() {
            if health.bytes >= budget.max_bytes_per_source {
                stop = Some(StopReason::Bytes);
                break 'sessions;
            }
            if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
                stop = Some(StopReason::Time);
                break 'sessions;
            }
            let Ok(value) = serde_json::from_slice::<Value>(line.bytes) else {
                continue;
            };
            match project_event(&value) {
                DshEventOutcome::Units(units) => {
                    let row_sha256 = sha256_hex(line.bytes);
                    let time = event_time(&value).unwrap_or(0);
                    let seq = event_seq(&value);
                    for unit in units {
                        if unit.text.trim().is_empty() {
                            continue;
                        }
                        if lexical_match(&fold_ascii(&unit.text), terms).is_some() {
                            pending.push(PendingHit {
                                line_index: line.index,
                                event_seq: seq,
                                pointer: unit.pointer,
                                range_sha256: sha256_hex(unit.text.as_bytes()),
                                row_sha256: row_sha256.clone(),
                                text: unit.text,
                                time,
                            });
                        }
                    }
                }
                DshEventOutcome::Empty => {}
                DshEventOutcome::Ignored { type_literal } => {
                    *ignored_types.entry(type_literal).or_insert(0) += 1;
                }
            }
        }
        for hit in pending {
            let Some(found) = lexical_match(&fold_ascii(&hit.text), terms) else {
                continue;
            };
            let text_bytes = hit.text.as_bytes();
            let (display, truncated) =
                snippet(&hit.text, found.first_byte, found.first_term_len, snippet_bytes);
            let session_id = meta.session_id.clone();
            hits.push(SourceHit {
                hit: HitRef {
                    source_id: source.source_id.clone(),
                    source_kind: crate::recall::contract::SOURCE_KIND_DSH.to_string(),
                    root_sha256: root_sha256(&source.root),
                    rel_path: session.rel_path.clone(),
                    schema_gen: SCHEMA_DSH_JSONL.to_string(),
                    locator: Locator::dsh(
                        &session_id,
                        hit.line_index,
                        &hit.pointer,
                        hit.event_seq,
                        Some(&session.rel_path),
                    ),
                    fingerprint: Fingerprint {
                        range_sha256: hit.range_sha256,
                        file_sha256: None,
                        row_sha256: Some(hit.row_sha256),
                    },
                    text_range: TextRange::from_span(text_bytes, 0, text_bytes.len()),
                },
                title: if meta.title.is_empty() {
                    session_id.clone()
                } else {
                    meta.title.clone()
                },
                snippet: display,
                snippet_truncated: truncated,
                score: None,
                score_kind: SCORE_KIND_NONE,
                rank_basis: crate::recall::contract::RANK_BASIS_LEXICAL_WINDOW,
                distinct_terms: found.distinct_terms,
                first_byte: found.first_byte,
                sort_time: if hit.time != 0 {
                    hit.time
                } else {
                    hit.line_index as i64
                },
                sort_id: format!("{}:{:020}", session_id, hit.line_index),
                related: Vec::new(),
                exact: None,
            });
        }
    }

    rank_hits(&mut hits);
    let mut detail = Map::new();
    let mut coverage = serde_json::json!({
        "scanned_sessions": scanned_sessions,
        "total_sessions": total_sessions,
    });
    if partial_tail > 0 {
        coverage["partial_tail"] = serde_json::json!(partial_tail);
    }
    if corrupt_sessions > 0 {
        coverage["corrupt_sessions"] = serde_json::json!(corrupt_sessions);
    }
    if unsupported_sessions > 0 {
        coverage["unsupported_sessions"] = serde_json::json!(unsupported_sessions);
    }
    if let Some(id) = session_filter {
        coverage["session_filter"] = serde_json::json!(id);
    }
    detail.insert("coverage".into(), coverage);
    if !ignored_types.is_empty() {
        let map: Map<String, Value> = ignored_types
            .iter()
            .map(|(key, count)| (key.clone(), serde_json::json!(count)))
            .collect();
        detail.insert("ignored_types".into(), Value::Object(map));
    }
    if let Some(reason) = stop {
        let mut budget_detail = Map::new();
        match reason {
            StopReason::Bytes => {
                budget_detail.insert("reason".into(), serde_json::json!(codes::BUDGET_BYTES));
                budget_detail.insert(
                    "consumed_bytes".into(),
                    serde_json::json!(health.bytes),
                );
            }
            StopReason::Time => {
                budget_detail.insert("reason".into(), serde_json::json!(codes::BUDGET_TIME));
                budget_detail.insert(
                    "elapsed_ms".into(),
                    serde_json::json!(started.elapsed().as_millis() as u64),
                );
            }
        }
        detail.insert("budget".into(), Value::Object(budget_detail));
    }

    let unsupported_only = unsupported_sessions > 0 && total_sessions == 0;
    let (status_value, code, message) = if health.skipped > 0 || health.errors > 0 {
        (
            status::PARTIAL,
            Some(codes::RECORD_SKIPPED),
            Some(format!(
                "scan lost records (skipped={}, errors={})",
                health.skipped, health.errors
            )),
        )
    } else if unsupported_only {
        (
            status::UNSUPPORTED,
            Some(codes::SCHEMA_UNSUPPORTED),
            Some(format!(
                "{unsupported_sessions} session(s) use an unknown DSH generation"
            )),
        )
    } else if unsupported_sessions > 0 {
        (
            status::PARTIAL,
            Some(codes::SCHEMA_UNSUPPORTED),
            Some(format!(
                "{unsupported_sessions} session(s) use an unknown DSH generation"
            )),
        )
    } else if let Some(reason) = stop {
        (
            status::TRUNCATED,
            Some(match reason {
                StopReason::Bytes => codes::BUDGET_BYTES,
                StopReason::Time => codes::BUDGET_TIME,
            }),
            Some("source budget exhausted before the scan completed".to_string()),
        )
    } else if hits.is_empty() {
        (status::EMPTY, None, None)
    } else {
        (status::OK, None, None)
    };

    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        hits,
        warnings: Vec::new(),
        error_message: message,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

/// Search reading from the configured root.
pub fn search(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    session_filter: Option<&str>,
) -> SourceOutcome {
    search_at(
        source,
        terms,
        budget,
        snippet_bytes,
        session_filter,
        Path::new(&source.root),
    )
}

/// True when `path` is inside both the configured root and the read root.
pub fn ensure_inside_root(source: &SourceConfig, read_root: &Path, carrier: &Path) -> bool {
    let root_str = read_root.to_string_lossy().to_string();
    rel_from_root(&root_str, carrier).is_some() && rel_from_root(&source.root, carrier).is_some()
}
