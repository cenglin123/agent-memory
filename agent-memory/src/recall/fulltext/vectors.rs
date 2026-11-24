//! L5b: conversation vector cache for one live source.
//!
//! Only user/assistant `text` units are embedded (tool inputs and outputs stay
//! lexical). The cache lives beside the full-text index at
//! `<cache_root>/live/<source_id>/vectors.bin`, is derived from that source's
//! own index only, and is deletable: deleting it and rebuilding produces the
//! same bytes.
//!
//! **Embedding input** carries structure, not just the body (the same lesson the
//! KB pipeline learned): the session title always leads, and an assistant unit
//! also carries the body of the nearest preceding user message in its turn
//! (capped at [`CONTEXT_USER_MAX_CHARS`] characters). The full-text text, the
//! snippet and the read-back range are untouched — only what the model sees.
//! The rule has its own version ([`EMBED_INPUT_RULE`]) inside the generation
//! key, so changing it re-embeds the whole source instead of mixing inputs.
//!
//! Vectors are int4 scalar quantized (`scale: f32` + `ceil(dim / 2)` nibble
//! bytes) through the same code the Archive generation uses. The header carries
//! the frozen generation key (model, dimension, normalization, backend, input
//! rule), the quantization name and the projector version; any difference is a
//! whole-source re-embed, never a mixed cache.
//!
//! Incremental reuse is two-layered: the cache stores a per-session fingerprint
//! (title + the ordered unit keys, text hashes and roles), so a refresh reads only
//! the bodies of the sessions whose input can have changed and takes every other
//! session's rows straight from the cache; and inside a recomputed session a row
//! whose embedding input hash already has a vector keeps it, so nothing is
//! embedded twice. A session rename changes the input of that session's units and
//! therefore re-embeds exactly them; an append re-embeds what is new.

use crate::recall::config::{RecallConfig, SourceConfig};
use crate::recall::contract::{ROLE_ASSISTANT, ROLE_USER};
use crate::recall::hash::sha256_hex;
use crate::recall::semantic::embed::{EmbeddingPlan, MAX_BATCH};
use crate::recall::semantic::index::{
    quantize_vector, quantized_row_bytes, VectorRow, VECTORS_QUANTIZATION,
};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

pub const VECTORS_MAGIC: [u8; 4] = *b"AMLV";
/// Version 2 added the projector version to the header and the embedding-input
/// hash to every row. Version 3 adds the per-session block: `session_id →
/// fingerprint of everything the embedding input depends on` (session title, the
/// ordered unit set and each unit's text hash and role). That block is what lets
/// a refresh recompute only the sessions whose input can have changed instead of
/// reading every body.
pub const VECTORS_VERSION: u16 = 3;
/// The previous layout. A build still reads its rows (they are content-addressed
/// by the embedding-input hash, so every row stays reusable); only the
/// per-session block is missing, so the upgrade round recomputes the input hashes
/// of every session once and embeds nothing it already has.
const VECTORS_VERSION_PREVIOUS: u16 = 2;

/// Version of the embedding-input construction (title + preceding user turn +
/// body, capped at [`EMBED_INPUT_MAX_CHARS`]). A different value means every
/// stored vector was built from a different string, so it re-embeds the whole
/// source.
pub const EMBED_INPUT_RULE: &str = "ctx-v2";

/// The whole embedding input is cut to this many characters. At most 4 UTF-8
/// bytes per character keeps it under the helper's 65536-byte text limit, and
/// the embedding model reads only the head of a long message anyway.
pub const EMBED_INPUT_MAX_CHARS: usize = 8000;

/// The preceding user message enters the assistant input up to this many
/// characters (the whole turn is not the unit; the body still leads the match).
pub const CONTEXT_USER_MAX_CHARS: usize = 300;

/// Live generation identity: the model identity plus the input-rule version.
/// The Archive semantic generation keeps using `plan.generation_key()` alone.
pub fn live_generation_key(plan: &EmbeddingPlan) -> String {
    format!("{};ctx={EMBED_INPUT_RULE}", plan.generation_key())
}

/// One embedding input: the session title, the preceding user turn (assistant
/// units only) and the unit body, newline separated.
pub fn embedding_input(role: &str, title: &str, preceding_user: Option<&str>, body: &str) -> String {
    let mut out = String::new();
    out.push_str(title);
    out.push('\n');
    if role == ROLE_ASSISTANT {
        if let Some(user) = preceding_user {
            let trimmed = user.trim();
            if !trimmed.is_empty() {
                out.push_str(&truncate_chars(trimmed, CONTEXT_USER_MAX_CHARS));
                out.push('\n');
            }
        }
    }
    out.push_str(body);
    truncate_chars(&out, EMBED_INPUT_MAX_CHARS)
}

/// Truncate by characters (never splitting one).
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// Report of one vector build (embedded into the `index` report detail).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VectorReport {
    /// `built|updated|unchanged|degraded|skipped`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub generation_key: String,
    pub projector_version: String,
    pub dimension: u64,
    pub units: u64,
    pub reused: u64,
    pub embedded: u64,
    pub duration_ms: u64,
    pub bytes: u64,
}

impl VectorReport {
    pub fn skipped(reason: &str) -> VectorReport {
        VectorReport {
            status: "skipped".to_string(),
            reason: Some(reason.to_string()),
            generation_key: String::new(),
            projector_version: super::EMBEDDING_PROJECTOR_VERSION.to_string(),
            dimension: 0,
            units: 0,
            reused: 0,
            embedded: 0,
            duration_ms: 0,
            bytes: 0,
        }
    }

    fn degraded(reason: String, plan: &EmbeddingPlan) -> VectorReport {
        VectorReport {
            status: "degraded".to_string(),
            reason: Some(reason),
            generation_key: live_generation_key(plan),
            projector_version: super::EMBEDDING_PROJECTOR_VERSION.to_string(),
            dimension: plan.spec.dimension,
            units: 0,
            reused: 0,
            embedded: 0,
            duration_ms: 0,
            bytes: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// File format
// ---------------------------------------------------------------------------

fn push_string(out: &mut Vec<u8>, value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() > u16::MAX as usize {
        return Err("header string is too long".to_string());
    }
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn read_string(bytes: &[u8], cursor: &mut usize) -> Result<String, ()> {
    if *cursor + 2 > bytes.len() {
        return Err(());
    }
    let length = u16::from_le_bytes([bytes[*cursor], bytes[*cursor + 1]]) as usize;
    *cursor += 2;
    if *cursor + length > bytes.len() {
        return Err(());
    }
    let text = std::str::from_utf8(&bytes[*cursor..*cursor + length])
        .map_err(|_| ())?
        .to_string();
    *cursor += length;
    Ok(text)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, ()> {
    if *cursor + 4 > bytes.len() {
        return Err(());
    }
    let value = u32::from_le_bytes([
        bytes[*cursor],
        bytes[*cursor + 1],
        bytes[*cursor + 2],
        bytes[*cursor + 3],
    ]);
    *cursor += 4;
    Ok(value)
}

#[derive(Debug)]
struct Header {
    generation_key: String,
    quantization: String,
    projector_version: String,
    dimension: usize,
    count: usize,
    /// Number of entries in the per-session block (0 for the previous layout).
    sessions: usize,
}

/// Parse the fixed header and stop **before** the per-session block, so a caller
/// that only needs the shape (the coverage counter) never has to read it.
fn parse_header_any(bytes: &[u8]) -> Result<(Header, usize, u16), &'static str> {
    if bytes.len() < 4 + 2 || bytes[..4] != VECTORS_MAGIC {
        return Err("vectors_corrupt");
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VECTORS_VERSION && version != VECTORS_VERSION_PREVIOUS {
        return Err("vectors_version_mismatch");
    }
    let mut cursor = 6;
    let generation_key = read_string(bytes, &mut cursor).map_err(|_| "vectors_corrupt")?;
    let quantization = read_string(bytes, &mut cursor).map_err(|_| "vectors_corrupt")?;
    let projector_version = read_string(bytes, &mut cursor).map_err(|_| "vectors_corrupt")?;
    let dimension = read_u32(bytes, &mut cursor).map_err(|_| "vectors_corrupt")? as usize;
    let count = read_u32(bytes, &mut cursor).map_err(|_| "vectors_corrupt")? as usize;
    let sessions = if version >= VECTORS_VERSION {
        read_u32(bytes, &mut cursor).map_err(|_| "vectors_corrupt")? as usize
    } else {
        0
    };
    if dimension == 0 || dimension > crate::recall::semantic::model::MAX_DIMENSION as usize {
        return Err("vectors_corrupt");
    }
    Ok((
        Header {
            generation_key,
            quantization,
            projector_version,
            dimension,
            count,
            sessions,
        },
        cursor,
        version,
    ))
}

/// The current layout only: an older file is a rebuild, never a silent
/// re-interpretation.
fn parse_header(bytes: &[u8]) -> Result<(Header, usize), &'static str> {
    let (header, cursor, version) = parse_header_any(bytes)?;
    if version != VECTORS_VERSION {
        return Err("vectors_version_mismatch");
    }
    Ok((header, cursor))
}

/// Step over the per-session block (`session_id`, fingerprint) pairs.
fn skip_sessions(bytes: &[u8], cursor: &mut usize, count: usize) -> Result<(), ()> {
    for _ in 0..count {
        read_string(bytes, cursor)?;
        read_string(bytes, cursor)?;
    }
    Ok(())
}

/// Reusable content row: the embedding-input hash plus its quantized bytes.
struct Entry {
    offset: usize,
    text_sha256: String,
}

/// A loaded cache. Rows are decoded lazily, one unit at a time, so a query never
/// materialises every vector.
pub struct VectorCache {
    pub generation_key: String,
    pub projector_version: String,
    pub dimension: usize,
    pub units: usize,
    bytes: Vec<u8>,
    row_bytes: usize,
    entries: BTreeMap<String, Entry>,
}

impl VectorCache {
    /// Load and validate one cache file against the configured model.
    ///
    /// Every failure is an explicit reason (`vectors_missing`,
    /// `vectors_unreadable`, `vectors_generation_mismatch`,
    /// `vectors_version_mismatch`, `vectors_projector_version_changed`,
    /// `vectors_quantization_mismatch`, `vectors_dimension_mismatch`,
    /// `vectors_corrupt`); the caller degrades the semantic branch and keeps the
    /// lexical one.
    pub fn load(path: &Path, plan: &EmbeddingPlan) -> Result<VectorCache, &'static str> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err("vectors_missing")
            }
            Err(_) => return Err("vectors_unreadable"),
        };
        let (header, mut cursor) = parse_header(&bytes)?;
        if header.generation_key != live_generation_key(plan) {
            return Err("vectors_generation_mismatch");
        }
        if header.projector_version != super::EMBEDDING_PROJECTOR_VERSION {
            return Err("projector_version_changed");
        }
        if header.quantization != VECTORS_QUANTIZATION {
            return Err("vectors_quantization_mismatch");
        }
        if header.dimension != plan.spec.dimension as usize {
            return Err("vectors_dimension_mismatch");
        }
        skip_sessions(&bytes, &mut cursor, header.sessions).map_err(|_| "vectors_corrupt")?;
        let row_bytes = quantized_row_bytes(header.dimension);
        let mut entries = BTreeMap::new();
        for _ in 0..header.count {
            let doc_key = read_string(&bytes, &mut cursor).map_err(|_| "vectors_corrupt")?;
            if cursor + 64 > bytes.len() {
                return Err("vectors_corrupt");
            }
            // The row keeps the *text* hash: it answers "is this vector still
            // about this unit's body?" without rebuilding the context.
            let text_sha256 = hex_from_bytes(&bytes[cursor..cursor + 32]);
            cursor += 64; // text hash + input hash
            if cursor + row_bytes > bytes.len() {
                return Err("vectors_corrupt");
            }
            entries.insert(doc_key, Entry { offset: cursor, text_sha256 });
            cursor += row_bytes;
        }
        if cursor != bytes.len() {
            return Err("vectors_corrupt");
        }
        Ok(VectorCache {
            generation_key: header.generation_key,
            projector_version: header.projector_version,
            dimension: header.dimension,
            units: entries.len(),
            bytes,
            row_bytes,
            entries,
        })
    }

    /// One unit's cosine similarity, or `None` when the unit has no usable row.
    /// A stored row whose text hash differs belongs to an older body (the
    /// full-text row changed after the last vector build) and is skipped.
    pub fn similarity(
        &self,
        doc_key: &str,
        text_sha256: &str,
        query: &[f32],
        query_norm: f32,
    ) -> Similarity {
        let Some(entry) = self.entries.get(doc_key) else {
            return Similarity::Missing;
        };
        if entry.text_sha256 != text_sha256 {
            return Similarity::Stale;
        }
        let Some(row) = VectorRow::from_bytes(
            &self.bytes[entry.offset..entry.offset + self.row_bytes],
            self.dimension,
        ) else {
            return Similarity::Missing;
        };
        Similarity::Value(row.similarity(query, query_norm))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Similarity {
    /// The cache has no row for this unit.
    Missing,
    /// The cache row was built from a different text.
    Stale,
    Value(f64),
}

/// How many vectors a cache file holds, when that file matches the configured
/// model and projection rules. Used by the `coverage` block; reads only the
/// header, never the rows.
pub fn cached_units(path: &Path, plan: &EmbeddingPlan) -> Option<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; 4096];
    let read = file.read(&mut buffer).ok()?;
    let (header, _) = parse_header(&buffer[..read]).ok()?;
    if header.generation_key != live_generation_key(plan)
        || header.quantization != VECTORS_QUANTIZATION
        || header.projector_version != super::EMBEDDING_PROJECTOR_VERSION
        || header.dimension != plan.spec.dimension as usize
    {
        return None;
    }
    Some(header.count as u64)
}

// ---------------------------------------------------------------------------
// Read-only digest (§G.1 oracle)
// ---------------------------------------------------------------------------

/// Normalised digest of one cache file. Floats never enter it: rows are
/// compared through their quantised bytes, keyed by the embedding-input hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VectorDigest {
    /// `ready` | `missing` | `unreadable` | `corrupt`.
    ///
    /// `missing` = no file; `unreadable` = the file exists but could not be read
    /// (after one retry) so its bytes were never parsed; `corrupt` = the bytes
    /// were read and are not this crate's cache.
    pub status: String,
    pub count: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub generation_key: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub dimension: u64,
    /// `doc_key → input_sha256`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub doc_key_input: BTreeMap<String, String>,
    /// `input_sha256 → sorted set of sha256(quantised row bytes)`. A set, not
    /// one value: two units may share an embedding input while a non
    /// deterministic backend produces different quantised bytes; every row is
    /// kept, so the comparison never depends on which row was parsed last.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub input_quantized: BTreeMap<String, BTreeSet<String>>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn digest_error(status: &str) -> VectorDigest {
    VectorDigest {
        status: status.to_string(),
        count: 0,
        generation_key: String::new(),
        dimension: 0,
        doc_key_input: BTreeMap::new(),
        input_quantized: BTreeMap::new(),
    }
}

/// Read a cache file for the digest.
///
/// A transient failure — a virus scanner holding the file, a network blip — is
/// retried once. If it persists the file is reported as `unreadable`, which is
/// **not** `corrupt`: nothing was parsed, so nothing is known about the bytes.
fn read_for_digest(path: &Path) -> Result<Vec<u8>, &'static str> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err("missing"),
        Err(_) => std::fs::read(path).map_err(|_| "unreadable"),
    }
}

/// Read-only digest of a cache file. A missing or unreadable file is a status,
/// never an error: the oracle compares statuses too.
pub fn digest_file(path: &Path) -> VectorDigest {
    let bytes = match read_for_digest(path) {
        Ok(bytes) => bytes,
        Err(status) => return digest_error(status),
    };
    let Ok((header, mut cursor)) = parse_header(&bytes) else {
        return digest_error("corrupt");
    };
    if skip_sessions(&bytes, &mut cursor, header.sessions).is_err() {
        return digest_error("corrupt");
    }
    let row_bytes = quantized_row_bytes(header.dimension);
    let mut doc_key_input = BTreeMap::new();
    let mut input_quantized: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for _ in 0..header.count {
        let Ok(doc_key) = read_string(&bytes, &mut cursor) else {
            return digest_error("corrupt");
        };
        if cursor + 64 > bytes.len() {
            return digest_error("corrupt");
        }
        // text hash is content of the full-text projection, not of the vector;
        // the input hash is what the vector was computed from.
        cursor += 32;
        let input_sha256 = hex_from_bytes(&bytes[cursor..cursor + 32]);
        cursor += 32;
        if cursor + row_bytes > bytes.len() {
            return digest_error("corrupt");
        }
        let row = &bytes[cursor..cursor + row_bytes];
        doc_key_input.insert(doc_key, input_sha256.clone());
        input_quantized
            .entry(input_sha256)
            .or_default()
            .insert(sha256_hex(row));
        cursor += row_bytes;
    }
    VectorDigest {
        status: "ready".to_string(),
        count: header.count as u64,
        generation_key: header.generation_key,
        dimension: header.dimension as u64,
        doc_key_input,
        input_quantized,
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// One conversation unit's metadata: everything the embedding input depends on
/// **except the body**. Selecting these columns never materialises `units.text`.
struct UnitMeta {
    doc_key: String,
    text_sha256: String,
    session_id: String,
    role: String,
    title: String,
}

impl UnitMeta {
    /// The title the embedding input leads with (`session_id` when the source has
    /// no title — exactly what [`embedding_input`] is called with below).
    fn effective_title(&self) -> &str {
        if self.title.is_empty() {
            &self.session_id
        } else {
            &self.title
        }
    }
}

/// The per-round metadata query backing the vector increment.
///
/// It reads the **narrow** `conversation_units` table only. The same information
/// lives in the FTS5 row, but reading any column of an FTS5 row reads its text
/// too, so a join into `units` costs the whole index in I/O every round. The
/// `title` column is a denormalised copy the writer maintains (see
/// `fulltext::migrate_narrow_columns`). The regression suite asserts this
/// statement's plan never touches `units`.
pub const CONVERSATION_METADATA_SQL: &str =
    "SELECT doc_key, text_sha256, session_id, role, title FROM conversation_units \
     ORDER BY session_id, ts, rowid";

/// `doc_key → (session_id, text_sha256, role, title)` for every conversation
/// unit, in the order the embedding input needs (`session_id`, `ts`, `rowid`).
///
/// This is the incremental baseline: it answers "which sessions can have a
/// different embedding input?" without reading a single body.
fn conversation_metadata(conn: &Connection) -> rusqlite::Result<Vec<UnitMeta>> {
    let mut statement = conn.prepare(CONVERSATION_METADATA_SQL)?;
    let rows = statement.query_map([], |row| {
        Ok(UnitMeta {
            doc_key: row.get(0)?,
            text_sha256: row.get(1)?,
            session_id: row.get(2)?,
            role: row.get(3)?,
            title: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        })
    })?;
    rows.collect()
}

/// The bodies of one session, in the same order as the metadata query. Only
/// sessions whose fingerprint changed are read.
fn session_bodies(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut statement = conn.prepare(
        "SELECT c.doc_key, u.text FROM conversation_units c JOIN units u ON u.rowid = c.rowid \
         WHERE c.session_id = ?1 ORDER BY c.ts, c.rowid",
    )?;
    let rows = statement.query_map(params![session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        ))
    })?;
    rows.collect()
}

/// One session's units plus the fingerprint of its embedding inputs.
struct SessionGroup {
    session_id: String,
    /// Slice of [`conversation_metadata`] this session owns.
    start: usize,
    end: usize,
    /// sha256 over the session title and, in order, every unit's `doc_key`,
    /// `text_sha256` and `role`. These are exactly the facts the embedding input
    /// is built from (the preceding user turn comes from an earlier unit of the
    /// same session), so equal fingerprints mean equal inputs — including the
    /// order, which is why an insertion in the middle changes it.
    fingerprint: String,
    /// True when the previous cache already holds this session's fingerprint and
    /// every one of its rows.
    reusable: bool,
}

fn session_groups(units: &[UnitMeta]) -> Vec<SessionGroup> {
    let mut groups: Vec<SessionGroup> = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        match groups.last_mut() {
            Some(group) if group.session_id == unit.session_id && group.end == index => {
                group.end = index + 1;
            }
            _ => groups.push(SessionGroup {
                session_id: unit.session_id.clone(),
                start: index,
                end: index + 1,
                fingerprint: String::new(),
                reusable: false,
            }),
        }
    }
    for group in &mut groups {
        let mut canonical = String::new();
        for unit in &units[group.start..group.end] {
            for field in [
                unit.effective_title(),
                unit.doc_key.as_str(),
                unit.text_sha256.as_str(),
                unit.role.as_str(),
            ] {
                // Length-prefixed fields: a `doc_key` already contains the unit
                // separator, so the boundary must be explicit.
                canonical.push_str(&field.len().to_string());
                canonical.push(':');
                canonical.push_str(field);
                canonical.push('\u{1f}');
            }
        }
        group.fingerprint = sha256_hex(canonical.as_bytes());
    }
    groups
}

/// One reusable row of the previous cache.
struct StoredRow {
    input_sha256: String,
    row: Vec<u8>,
}

/// Everything the previous cache offers this round.
#[derive(Default)]
struct PreviousCache {
    /// `session_id → session fingerprint` (empty for the previous layout).
    sessions: BTreeMap<String, String>,
    /// `doc_key → its stored row`.
    rows: BTreeMap<String, StoredRow>,
    /// `embedding input hash → quantised row` (content-addressed reuse).
    by_input: BTreeMap<String, Vec<u8>>,
    /// The header matches this model, quantization, dimension and input rule, so
    /// the rows may be reused (only the session block needs the current layout).
    compatible: bool,
}

/// Read the previous cache. A file from the previous layout contributes its rows
/// and no session block (the upgrade round recomputes every fingerprint); a file
/// from another generation contributes nothing.
fn load_previous(bytes: Option<&[u8]>, plan: &EmbeddingPlan) -> PreviousCache {
    let mut cache = PreviousCache::default();
    let Some(bytes) = bytes else {
        return cache;
    };
    let Ok((header, mut cursor, version)) = parse_header_any(bytes) else {
        return cache;
    };
    if header.generation_key != live_generation_key(plan)
        || header.quantization != VECTORS_QUANTIZATION
        || header.dimension != plan.spec.dimension as usize
        || header.projector_version != super::EMBEDDING_PROJECTOR_VERSION
    {
        return cache;
    }
    let row_bytes = quantized_row_bytes(header.dimension);
    let parsed = (|| -> Result<(), ()> {
        if version >= VECTORS_VERSION {
            for _ in 0..header.sessions {
                let session = read_string(bytes, &mut cursor)?;
                let fingerprint = read_string(bytes, &mut cursor)?;
                cache.sessions.insert(session, fingerprint);
            }
        }
        for _ in 0..header.count {
            let doc_key = read_string(bytes, &mut cursor)?;
            if cursor + 64 > bytes.len() {
                return Err(());
            }
            // The text hash is content of the full-text projection, not of the
            // vector; the session fingerprint already covers it.
            cursor += 32;
            let input_sha256 = hex_from_bytes(&bytes[cursor..cursor + 32]);
            cursor += 32;
            if cursor + row_bytes > bytes.len() {
                return Err(());
            }
            let row = bytes[cursor..cursor + row_bytes].to_vec();
            cursor += row_bytes;
            // A row is addressed by its input hash: two units that share an
            // embedding input share the row.
            cache
                .by_input
                .entry(input_sha256.clone())
                .or_insert_with(|| row.clone());
            cache.rows.insert(
                doc_key,
                StoredRow {
                    input_sha256,
                    row,
                },
            );
        }
        Ok(())
    })();
    if parsed.is_err() {
        return PreviousCache::default();
    }
    cache.compatible = true;
    cache
}

fn serialize(
    generation_key: &str,
    dimension: usize,
    sessions: &BTreeMap<String, String>,
    rows: &[(&str, &str, &str, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.extend_from_slice(&VECTORS_MAGIC);
    out.extend_from_slice(&VECTORS_VERSION.to_le_bytes());
    push_string(&mut out, generation_key)?;
    push_string(&mut out, VECTORS_QUANTIZATION)?;
    push_string(&mut out, super::EMBEDDING_PROJECTOR_VERSION)?;
    out.extend_from_slice(&(dimension as u32).to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&(sessions.len() as u32).to_le_bytes());
    for (session, fingerprint) in sessions {
        push_string(&mut out, session)?;
        push_string(&mut out, fingerprint)?;
    }
    for (doc_key, text_sha256, input_sha256, row) in rows {
        push_string(&mut out, doc_key)?;
        let text = decode_hex(text_sha256).ok_or_else(|| "bad text hash".to_string())?;
        out.extend_from_slice(&text);
        let input = decode_hex(input_sha256).ok_or_else(|| "bad input hash".to_string())?;
        out.extend_from_slice(&input);
        out.extend_from_slice(row);
    }
    Ok(out)
}

fn hex_from_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = Vec::with_capacity(32);
    for chunk in hex.as_bytes().chunks(2) {
        let high = (chunk[0] as char).to_digit(16)?;
        let low = (chunk[1] as char).to_digit(16)?;
        out.push(((high << 4) | low) as u8);
    }
    Some(out)
}

/// Build (or refresh) one source's vector cache from its full-text index.
///
/// Failures are reported, never propagated: a missing model leaves the
/// full-text index usable and marks the vector part `degraded`.
///
/// # Per-session incrementality
///
/// 1. Read `doc_key → (session_id, text_sha256, role, title)` from the index —
///    metadata only, never a body.
/// 2. Fingerprint each session (title + ordered unit keys / text hashes / roles)
///    and compare it with the previous cache's block. A session whose fingerprint
///    and rows are all present is **reused**: its rows are the same bytes for the
///    same inputs, so nothing is read and nothing is embedded.
/// 3. Read the bodies of the remaining sessions (one query per session) and
///    recompute their embedding inputs. The whole session is recomputed, so a
///    changed earlier user turn also refreshes the replies whose context it is —
///    that is the "context drift" case, and it is why the grain is a session and
///    not a unit.
/// 4. Embed only inputs that no cached row covers (content-addressed by the input
///    hash, across sessions).
pub fn build(config: &RecallConfig, source: &SourceConfig, plan: &EmbeddingPlan) -> VectorReport {
    let started = Instant::now();
    let Ok(cache_root) = super::configured_cache_root(config) else {
        return VectorReport::degraded("cache_root_missing".to_string(), plan);
    };
    let (conn, _path, state) = super::open_for_query(config, source);
    if state != super::IndexState::Ready {
        return VectorReport::degraded("index_not_ready".to_string(), plan);
    }
    // One read snapshot for both passes: the metadata pass and the body pass
    // must describe the same index even if a concurrent `index` run commits in
    // between (it waits for this read transaction).
    let snapshot = conn.unchecked_transaction().ok();
    let units = match conversation_metadata(&conn) {
        Ok(units) => units,
        Err(error) => {
            return VectorReport::degraded(format!("index_query_failed: {error}"), plan)
        }
    };
    let mut groups = session_groups(&units);

    let path = super::vectors_path(&cache_root, &source.source_id);
    let previous_bytes = std::fs::read(&path).ok();
    let mut previous = load_previous(previous_bytes.as_deref(), plan);
    let previous_compatible = previous.compatible;

    // Decide the grain before reading anything: a session is reusable when the
    // previous cache recorded the same fingerprint and still holds every row.
    let mut sessions_reused = 0u64;
    let mut sessions_recomputed = 0u64;
    for group in &mut groups {
        let identical = previous
            .sessions
            .get(&group.session_id)
            .map(|stored| stored == &group.fingerprint)
            .unwrap_or(false);
        let rows_present = units[group.start..group.end]
            .iter()
            .all(|unit| previous.rows.contains_key(&unit.doc_key));
        group.reusable = identical && rows_present;
        if group.reusable {
            sessions_reused += 1;
        } else {
            sessions_recomputed += 1;
        }
    }

    // Recompute the inputs of the sessions that changed (bodies read: those
    // sessions only).
    let mut input_hashes: Vec<Option<String>> = vec![None; units.len()];
    let mut pending: BTreeMap<String, String> = BTreeMap::new();
    let mut units_read = 0u64;
    for group in &groups {
        if group.reusable {
            continue;
        }
        let bodies = match session_bodies(&conn, &group.session_id) {
            Ok(bodies) => bodies,
            Err(error) => {
                return VectorReport::degraded(format!("index_query_failed: {error}"), plan)
            }
        };
        if bodies.len() != group.end - group.start {
            // The metadata and the body reads are one snapshot, so this can only
            // mean the index changed under an unusable transaction: refuse to
            // write a cache that mixes two generations.
            return VectorReport::degraded("index_changed_during_build".to_string(), plan);
        }
        units_read += bodies.len() as u64;
        let mut preceding_user: Option<String> = None;
        for (offset, (unit, (doc_key, body))) in units[group.start..group.end]
            .iter()
            .zip(bodies)
            .enumerate()
        {
            debug_assert_eq!(doc_key, unit.doc_key, "body rows stay in metadata order");
            let input = embedding_input(
                &unit.role,
                unit.effective_title(),
                preceding_user.as_deref(),
                &body,
            );
            let hash = sha256_hex(input.as_bytes());
            if !previous.by_input.contains_key(&hash) && !pending.contains_key(&hash) {
                pending.insert(hash.clone(), input);
            }
            input_hashes[group.start + offset] = Some(hash);
            if unit.role == ROLE_USER {
                preceding_user = Some(body);
            }
        }
    }

    let mut degraded: Option<String> = None;
    // §A: the vector stage's read side — the units whose bodies were read (not
    // the whole corpus) and the session split.
    crate::cost::counter("vectors.units_read", units_read);
    crate::cost::counter("vectors.sessions_recomputed", sessions_recomputed);
    crate::cost::counter("vectors.sessions_reused", sessions_reused);
    crate::cost::counter(
        "vectors.cache_bytes_read",
        previous_bytes
            .as_ref()
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(0),
    );

    let mut embedded_inputs: BTreeSet<String> = BTreeSet::new();
    let hashes: Vec<String> = pending.keys().cloned().collect();
    for chunk in hashes.chunks(MAX_BATCH) {
        let texts: Vec<String> = chunk
            .iter()
            .map(|hash| pending.get(hash).cloned().unwrap_or_default())
            .collect();
        match plan.embed_batch(&texts) {
            Ok(vectors) => {
                for (hash, vector) in chunk.iter().zip(vectors) {
                    previous
                        .by_input
                        .insert(hash.clone(), quantize_vector(&vector));
                    embedded_inputs.insert(hash.clone());
                }
            }
            Err(error) => {
                degraded = Some(match error.detail {
                    Some(detail) => format!("{}: {}", error.reason, detail),
                    None => error.reason,
                });
                break;
            }
        }
    }

    let mut rows: Vec<(&str, &str, &str, Vec<u8>)> = Vec::with_capacity(units.len());
    let mut embedded = 0u64;
    let mut reused = 0u64;
    for (index, unit) in units.iter().enumerate() {
        match &input_hashes[index] {
            Some(hash) => {
                let Some(row) = previous.by_input.get(hash) else {
                    continue;
                };
                if embedded_inputs.contains(hash) {
                    embedded += 1;
                } else {
                    reused += 1;
                }
                rows.push((&unit.doc_key, &unit.text_sha256, hash.as_str(), row.clone()));
            }
            None => {
                // Not recomputed: the session fingerprint proves the input (and
                // therefore the stored row) is the current one.
                let Some(stored) = previous.rows.get(&unit.doc_key) else {
                    continue;
                };
                reused += 1;
                rows.push((
                    &unit.doc_key,
                    &unit.text_sha256,
                    stored.input_sha256.as_str(),
                    stored.row.clone(),
                ));
            }
        }
    }
    crate::cost::counter("vectors.rows_reused", reused);
    crate::cost::counter("vectors.embedded", embedded);
    // Release the snapshot before writing (a query connection holds a read lock
    // only while the transaction is open).
    drop(snapshot);

    if let Some(reason) = degraded {
        // A partial cache would silently under-cover the corpus. Keep whatever
        // valid cache exists (query-time content hashes reject changed rows) and
        // report the failure; the full-text index stays usable either way.
        return VectorReport {
            status: "degraded".to_string(),
            reason: Some(reason),
            generation_key: live_generation_key(plan),
            projector_version: super::EMBEDDING_PROJECTOR_VERSION.to_string(),
            dimension: plan.spec.dimension,
            units: rows.len() as u64,
            reused,
            embedded,
            duration_ms: started.elapsed().as_millis() as u64,
            bytes: previous_bytes
                .as_ref()
                .map(|bytes| bytes.len() as u64)
                .unwrap_or(0),
        };
    }

    let sessions: BTreeMap<String, String> = groups
        .iter()
        .map(|group| (group.session_id.clone(), group.fingerprint.clone()))
        .collect();
    let encoded = match serialize(
        &live_generation_key(plan),
        plan.spec.dimension as usize,
        &sessions,
        &rows,
    ) {
        Ok(encoded) => encoded,
        Err(reason) => {
            return VectorReport::degraded(format!("vectors_encode_failed: {reason}"), plan)
        }
    };
    let unchanged = previous_bytes.as_deref() == Some(encoded.as_slice());
    if !unchanged {
        // The write goes through `cache_store`, the only writer outside
        // `src/recall` that a cache build is allowed to use.
        let directory = super::sanitize_source_id(&source.source_id);
        let components = [super::LIVE_DIR, directory.as_str(), super::VECTORS_FILENAME];
        if let Err(error) = crate::cache_store::write_atomic(&cache_root, &components, &encoded) {
            return VectorReport::degraded(format!("vectors_write_failed: {error}"), plan);
        }
    }

    // A file from another generation is not "updated": every vector is new.
    let status = if unchanged {
        "unchanged"
    } else if previous_compatible {
        "updated"
    } else {
        "built"
    };
    VectorReport {
        status: status.to_string(),
        reason: None,
        generation_key: live_generation_key(plan),
        projector_version: super::EMBEDDING_PROJECTOR_VERSION.to_string(),
        dimension: plan.spec.dimension,
        units: rows.len() as u64,
        reused,
        embedded,
        duration_ms: started.elapsed().as_millis() as u64,
        bytes: encoded.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_input_carries_title_and_the_preceding_user_turn() {
        let user = embedding_input(ROLE_USER, "会话标题", Some("ignored for users"), "本单元正文");
        assert_eq!(user, "会话标题\n本单元正文");
        let assistant = embedding_input(
            ROLE_ASSISTANT,
            "会话标题",
            Some("第一问"),
            "本单元正文",
        );
        assert_eq!(assistant, "会话标题\n第一问\n本单元正文");
        // No preceding user turn yet: title + body, never a dangling separator.
        let orphan = embedding_input(ROLE_ASSISTANT, "会话标题", None, "本单元正文");
        assert_eq!(orphan, "会话标题\n本单元正文");
        let blank = embedding_input(ROLE_ASSISTANT, "标题", Some("   "), "正文");
        assert_eq!(blank, "标题\n正文");
    }

    #[test]
    fn the_preceding_user_turn_is_capped_by_characters() {
        let long = "问题".repeat(400);
        let input = embedding_input(ROLE_ASSISTANT, "标题", Some(&long), "正文");
        let context = input.lines().nth(1).unwrap();
        assert_eq!(context.chars().count(), CONTEXT_USER_MAX_CHARS);
        assert!(input.ends_with("\n正文"));
    }

    #[test]
    fn the_whole_input_is_capped_below_the_helper_byte_limit() {
        let body = "长".repeat(30_000);
        let input = embedding_input(ROLE_ASSISTANT, "标题", Some("问"), &body);
        assert_eq!(input.chars().count(), EMBED_INPUT_MAX_CHARS);
        assert!(input.len() <= 65_536);
        assert!(input.starts_with("标题
问
长"));
    }

    #[test]
    fn the_input_rule_is_part_of_the_live_generation_key() {
        let plan = EmbeddingPlan::for_spec(crate::recall::semantic::model::EmbeddingModelSpec {
            model_id: "fake-hash-v1".to_string(),
            dimension: 16,
            normalization: crate::recall::semantic::model::Normalization::L2,
            backend: crate::recall::semantic::model::EmbeddingBackend::Fake,
            model_path: None,
            endpoint: None,
        });
        let key = live_generation_key(&plan);
        assert!(key.ends_with(";ctx=ctx-v2"), "{key}");
        assert!(key.starts_with(&plan.generation_key()), "{key}");
        assert_ne!(key, plan.generation_key());
    }
}
