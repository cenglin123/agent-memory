//! Semantic retrieval index: vector artifact layout, validation and scoring.
//!
//! The index lives in the same derived generation as the lexical skeleton
//! (`derived/generations/g<N>/retrieval/`) and is addressed by the frozen
//! generation key. It is validated as a whole before one vector is used:
//! schema, unit parser, replay identity, embedding identity, access labels and
//! the vectors file hash. Any mismatch is an explicit status, never a silent
//! parse of another generation's vectors.
//!
//! Vectors are stored as int4 scalar quantization (two signed 4-bit values per
//! byte, `-7..=7`, plus one f32 scale per unit) so the index stays inside the P0
//! size budget at a larger dimension: one unit costs `4 + ceil(dim / 2)` bytes
//! (260 B at dimension 512, the same as int8 at dimension 256). The actual
//! dimension still enters the generation key and the quantization is frozen in
//! the generation metadata, so a storage change forces a new generation.

use crate::recall::hash::sha256_hex;
use crate::recall::semantic::embed::EmbeddingPlan;
use crate::recall::semantic::units::UNIT_PARSER;
use crate::replay::ReplayIdentity;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const RETRIEVAL_INDEX_SCHEMA: &str = "agent-memory/retrieval-index@3";
pub const RETRIEVAL_UNIT_PARSER: &str = UNIT_PARSER;
pub const VECTORS_QUANTIZATION: &str = "int4";
pub const VECTOR_SCALE_BYTES: usize = 4;
/// Signed 4-bit range stored in one nibble.
pub const VECTOR_INT4_MIN: i8 = -7;
pub const VECTOR_INT4_MAX: i8 = 7;
pub const RRF_K: f64 = 60.0;

pub fn retrieval_index_rel(generation: u64) -> String {
    format!("derived/generations/g{generation}/retrieval/index.json")
}

pub fn vectors_rel(generation: u64) -> String {
    format!("derived/generations/g{generation}/retrieval/vectors.bin")
}

pub fn units_rel(generation: u64) -> String {
    format!("derived/generations/g{generation}/retrieval/units.bin")
}

/// Binary unit table. The table replaces the per-unit JSON array: fixed-width
/// hashes and small integer ids keep short-session indexes inside the size
/// budget without moving anything outside `retrieval/`.
pub const UNITS_MAGIC: &[u8; 4] = b"AMU1";
pub const UNITS_VERSION: u16 = 1;
pub const UNITS_SCHEMA: &str = "agent-memory/retrieval-units@1";
const NO_LINE: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRow {
    pub unit_sha256: String,
    pub unit_type: String,
    pub source_key: String,
    pub record_sha256: String,
    pub line: Option<u64>,
    pub text_bytes: u64,
    pub turn: u32,
    pub access: Vec<String>,
}

fn decode_hex(hex: &str, size: usize) -> Result<Vec<u8>, String> {
    if hex.len() != size * 2
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("hash must be {} lowercase hex digits", size * 2));
    }
    let mut out = Vec::with_capacity(size);
    for chunk in hex.as_bytes().chunks(2) {
        let high = (chunk[0] as char).to_digit(16).ok_or("bad hex")?;
        let low = (chunk[1] as char).to_digit(16).ok_or("bad hex")?;
        out.push(((high << 4) | low) as u8);
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

fn push_table(out: &mut Vec<u8>, values: &[String]) -> Result<(), String> {
    if values.len() > u16::MAX as usize {
        return Err("table has too many entries".to_string());
    }
    out.extend_from_slice(&(values.len() as u16).to_le_bytes());
    for value in values {
        if value.len() > u16::MAX as usize {
            return Err("table entry is too long".to_string());
        }
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    Ok(())
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, String> {
    if *cursor + 2 > bytes.len() {
        return Err("truncated u16".to_string());
    }
    let value = u16::from_le_bytes([bytes[*cursor], bytes[*cursor + 1]]);
    *cursor += 2;
    Ok(value)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    if *cursor + 4 > bytes.len() {
        return Err("truncated u32".to_string());
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

fn read_table(bytes: &[u8], cursor: &mut usize) -> Result<Vec<String>, String> {
    let count = read_u16(bytes, cursor)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let length = read_u16(bytes, cursor)? as usize;
        if *cursor + length > bytes.len() {
            return Err("truncated table entry".to_string());
        }
        let text = std::str::from_utf8(&bytes[*cursor..*cursor + length])
            .map_err(|_| "table entry is not UTF-8".to_string())?;
        out.push(text.to_string());
        *cursor += length;
    }
    Ok(out)
}

pub fn encode_unit_table(rows: &[UnitRow]) -> Result<Vec<u8>, String> {
    let mut labels: Vec<String> = Vec::new();
    let mut types: Vec<String> = Vec::new();
    let index_of = |table: &mut Vec<String>, value: &str| -> Result<u16, String> {
        if let Some(index) = table.iter().position(|item| item == value) {
            return Ok(index as u16);
        }
        if table.len() >= u16::MAX as usize {
            return Err("string table overflow".to_string());
        }
        table.push(value.to_string());
        Ok((table.len() - 1) as u16)
    };
    let mut encoded_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let type_index = index_of(&mut types, &row.unit_type)?;
        if row.access.len() > u8::MAX as usize {
            return Err("too many access labels on one unit".to_string());
        }
        let mut access_indexes = Vec::with_capacity(row.access.len());
        for label in &row.access {
            let index = index_of(&mut labels, label)?;
            if index > u8::MAX as u16 {
                return Err("too many access labels".to_string());
            }
            access_indexes.push(index as u8);
        }
        let mut encoded = Vec::with_capacity(96 + access_indexes.len());
        encoded.extend_from_slice(&decode_hex(&row.unit_sha256, 32)?);
        encoded.extend_from_slice(&decode_hex(&row.record_sha256, 32)?);
        encoded.extend_from_slice(&decode_hex(&row.source_key, 16)?);
        encoded.extend_from_slice(
            &u32::try_from(row.line.unwrap_or(NO_LINE as u64))
                .map_err(|_| "line out of range".to_string())?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(
            &u32::try_from(row.text_bytes)
                .map_err(|_| "text_bytes out of range".to_string())?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(&row.turn.to_le_bytes());
        encoded.extend_from_slice(&type_index.to_le_bytes());
        encoded.push(access_indexes.len() as u8);
        encoded.extend_from_slice(&access_indexes);
        encoded_rows.push(encoded);
    }

    if rows.len() > u32::MAX as usize {
        return Err("too many units".to_string());
    }
    let mut out = Vec::new();
    out.extend_from_slice(UNITS_MAGIC);
    out.extend_from_slice(&UNITS_VERSION.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    push_table(&mut out, &labels)?;
    push_table(&mut out, &types)?;
    for row in encoded_rows {
        out.extend_from_slice(&row);
    }
    Ok(out)
}

pub fn decode_unit_table(bytes: &[u8]) -> Result<Vec<UnitRow>, String> {
    if bytes.len() < 10 || &bytes[..4] != UNITS_MAGIC {
        return Err("unit table magic mismatch".to_string());
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != UNITS_VERSION {
        return Err("unit table version mismatch".to_string());
    }
    let mut cursor = 6;
    let count = read_u32(bytes, &mut cursor)? as usize;
    let labels = read_table(bytes, &mut cursor)?;
    let types = read_table(bytes, &mut cursor)?;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        if cursor + 32 + 32 + 16 + 4 + 4 + 4 + 2 + 1 > bytes.len() {
            return Err("truncated unit row".to_string());
        }
        let unit_sha256 = hex_from_bytes(&bytes[cursor..cursor + 32]);
        cursor += 32;
        let record_sha256 = hex_from_bytes(&bytes[cursor..cursor + 32]);
        cursor += 32;
        let source_key = hex_from_bytes(&bytes[cursor..cursor + 16]);
        cursor += 16;
        let line_raw = read_u32(bytes, &mut cursor)?;
        let text_bytes = read_u32(bytes, &mut cursor)? as u64;
        let turn = read_u32(bytes, &mut cursor)?;
        let type_index = read_u16(bytes, &mut cursor)? as usize;
        if type_index >= types.len() {
            return Err("unit type index out of range".to_string());
        }
        let access_count = bytes[cursor] as usize;
        cursor += 1;
        if cursor + access_count > bytes.len() {
            return Err("truncated access labels".to_string());
        }
        let mut access = Vec::with_capacity(access_count);
        for _ in 0..access_count {
            let index = bytes[cursor] as usize;
            cursor += 1;
            let label = labels
                .get(index)
                .ok_or_else(|| "access label index out of range".to_string())?;
            access.push(label.clone());
        }
        rows.push(UnitRow {
            unit_sha256,
            unit_type: types[type_index].clone(),
            source_key,
            record_sha256,
            line: if line_raw == NO_LINE {
                None
            } else {
                Some(line_raw as u64)
            },
            text_bytes,
            turn,
            access,
        });
    }
    if cursor != bytes.len() {
        return Err("trailing bytes in unit table".to_string());
    }
    Ok(rows)
}

/// Bytes one quantized unit vector occupies: `f32` scale plus two 4-bit values
/// per byte (`ceil(dimension / 2)` bytes).
pub fn quantized_row_bytes(dimension: usize) -> usize {
    VECTOR_SCALE_BYTES + dimension.div_ceil(2)
}

fn quantize_nibble(value: f32, scale: f32) -> u8 {
    let scaled = (value / scale)
        .round()
        .clamp(VECTOR_INT4_MIN as f32, VECTOR_INT4_MAX as f32) as i8;
    (scaled as u8) & 0x0f
}

fn decode_nibble(byte: u8, high: bool) -> i8 {
    let raw = if high { byte >> 4 } else { byte & 0x0f };
    // Sign-extend the 4-bit two's-complement value (`9` -> `-7`).
    ((raw as i8) << 4) >> 4
}

/// One int4-quantized unit vector: `scale` (f32 LE) followed by
/// `ceil(dimension / 2)` bytes. The value for even dimensions sits in the low
/// nibble, the odd one in the high nibble; the unused high nibble of an odd
/// dimension is zero.
pub fn quantize_vector(vector: &[f32]) -> Vec<u8> {
    let max = vector
        .iter()
        .fold(0.0f32, |acc, value| acc.max(value.abs()));
    let scale = if max == 0.0 {
        1.0
    } else {
        max / VECTOR_INT4_MAX as f32
    };
    let mut out = Vec::with_capacity(quantized_row_bytes(vector.len()));
    out.extend_from_slice(&scale.to_le_bytes());
    for pair in vector.chunks(2) {
        let low = quantize_nibble(pair[0], scale);
        let high = pair
            .get(1)
            .map(|value| quantize_nibble(*value, scale))
            .unwrap_or(0);
        out.push(low | (high << 4));
    }
    out
}

#[derive(Debug, Clone)]
pub struct VectorRow {
    scale: f32,
    values: Vec<i8>,
    norm: f32,
}

impl VectorRow {
    /// Decode one int4 row. Shared with the live-source vector cache (L5b),
    /// which stores rows in exactly this layout.
    pub fn from_bytes(bytes: &[u8], dimension: usize) -> Option<Self> {
        if dimension == 0 || bytes.len() != quantized_row_bytes(dimension) {
            return None;
        }
        let scale = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let packed = &bytes[VECTOR_SCALE_BYTES..];
        let values: Vec<i8> = (0..dimension)
            .map(|index| decode_nibble(packed[index / 2], index % 2 == 1))
            .collect();
        let norm = values
            .iter()
            .map(|value| {
                let scaled = *value as f32 * scale;
                scaled * scaled
            })
            .sum::<f32>()
            .sqrt();
        Some(VectorRow {
            scale,
            values,
            norm,
        })
    }

    /// Cosine similarity. Zero vectors score 0 instead of `NaN`.
    pub fn similarity(&self, query: &[f32], query_norm: f32) -> f64 {
        if self.norm == 0.0 || query_norm == 0.0 || query.len() != self.values.len() {
            return 0.0;
        }
        let dot: f32 = self
            .values
            .iter()
            .zip(query.iter())
            .map(|(value, query)| *value as f32 * self.scale * *query)
            .sum();
        (dot as f64) / (self.norm as f64 * query_norm as f64)
    }
}

pub fn vector_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticIndexStatus {
    Available,
    Degraded,
}

#[derive(Debug, Clone)]
pub struct SemanticIndex {
    pub status: SemanticIndexStatus,
    pub reason: Option<String>,
    pub generation: Option<u64>,
    pub generation_key: Option<String>,
    pub dimension: usize,
    pub units: usize,
    pub access: Vec<String>,
    rows: Vec<VectorRow>,
    lookup: BTreeMap<String, usize>,
}

impl SemanticIndex {
    fn degraded(reason: &str) -> Self {
        SemanticIndex {
            status: SemanticIndexStatus::Degraded,
            reason: Some(reason.to_string()),
            generation: None,
            generation_key: None,
            dimension: 0,
            units: 0,
            access: Vec::new(),
            rows: Vec::new(),
            lookup: BTreeMap::new(),
        }
    }

    pub fn available(&self) -> bool {
        self.status == SemanticIndexStatus::Available
    }

    pub fn status_str(&self) -> &'static str {
        match self.status {
            SemanticIndexStatus::Available => "available",
            SemanticIndexStatus::Degraded => "degraded",
        }
    }

    /// Row for one unit identity, if this generation indexed it.
    pub fn row(&self, unit_sha256: &str) -> Option<&VectorRow> {
        self.lookup.get(unit_sha256).and_then(|index| self.rows.get(*index))
    }
}

/// Read one generation's retrieval index document.
pub fn read_index_at(read_root: &Path, generation: u64) -> Result<Value, &'static str> {
    let path = read_root
        .join(retrieval_index_rel(generation).replace('/', std::path::MAIN_SEPARATOR_STR));
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("index_absent")
        }
        Err(_) => return Err("index_unreadable"),
    };
    let index: Value = serde_json::from_slice(&raw).map_err(|_| "index_unreadable")?;
    if index.get("schema").and_then(Value::as_str) != Some(RETRIEVAL_INDEX_SCHEMA) {
        return Err("index_schema_mismatch");
    }
    if index.get("unitParser").and_then(Value::as_str) != Some(RETRIEVAL_UNIT_PARSER) {
        return Err("index_unit_parser_mismatch");
    }
    let identity = index
        .get("identity")
        .and_then(|identity| serde_json::from_value::<ReplayIdentity>(identity.clone()).ok());
    if identity.as_ref() != Some(&ReplayIdentity::current()) {
        return Err("index_identity_mismatch");
    }
    Ok(index)
}

/// The current derived generation number and its retrieval index document.
///
/// Shared by the session mapping (which must also honour `index.access`) and the
/// semantic loader. `None` means there is no readable derived generation.
pub fn read_current_index(read_root: &Path) -> Result<Option<(u64, Value)>, &'static str> {
    let state_path = read_root.join("derived").join("state.json");
    let raw = match std::fs::read(&state_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("index_unreadable"),
    };
    let state: Value = serde_json::from_slice(&raw).map_err(|_| "index_unreadable")?;
    let Some(generations) = state.get("generations").and_then(Value::as_array) else {
        return Ok(None);
    };
    let Some(generation) = generations
        .iter()
        .max_by_key(|generation| generation.get("generation").and_then(Value::as_u64).unwrap_or(0))
    else {
        return Ok(None);
    };
    let number = generation.get("generation").and_then(Value::as_u64);
    let Some(number) = number else {
        return Ok(None);
    };
    if generation.get("status").and_then(Value::as_str) == Some("rebuilding") {
        return Err("index_rebuilding");
    }
    let identity_ok = generation
        .get("identity")
        .and_then(|identity| serde_json::from_value::<ReplayIdentity>(identity.clone()).ok())
        .map(|identity| identity == ReplayIdentity::current())
        .unwrap_or(false);
    if !identity_ok {
        return Err("index_identity_mismatch");
    }
    let index = read_index_at(read_root, number)?;
    Ok(Some((number, index)))
}

/// Load and validate the semantic generation for one query.
///
/// Generations are scanned newest-first and the first one whose generator key
/// matches the configured model is used. A model switch therefore allocates a new
/// generation while an older model's generation stays readable. `domain` is the
/// effective access domain: an index whose frozen access labels do not contain it
/// is not used (fail-closed), independent of the record-level predicate.
pub fn load(read_root: &Path, domain: &str, plan: &EmbeddingPlan) -> SemanticIndex {
    let state_path = read_root.join("derived").join("state.json");
    let raw = match std::fs::read(&state_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SemanticIndex::degraded("semantic_index_absent")
        }
        Err(_) => return SemanticIndex::degraded("index_unreadable"),
    };
    let state: Value = match serde_json::from_slice(&raw) {
        Ok(state) => state,
        Err(_) => return SemanticIndex::degraded("index_unreadable"),
    };
    let Some(generations) = state.get("generations").and_then(Value::as_array) else {
        return SemanticIndex::degraded("semantic_index_absent");
    };
    let mut ordered: Vec<&Value> = generations.iter().collect();
    ordered.sort_by_key(|generation| {
        std::cmp::Reverse(
            generation
                .get("generation")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
    });
    let wanted = plan.generation_key();
    let mut absent_reason = "semantic_index_absent";
    for generation in ordered {
        let Some(number) = generation.get("generation").and_then(Value::as_u64) else {
            continue;
        };
        if generation.get("status").and_then(Value::as_str) == Some("rebuilding") {
            absent_reason = "index_rebuilding";
            continue;
        }
        let identity_ok = generation
            .get("identity")
            .and_then(|identity| serde_json::from_value::<ReplayIdentity>(identity.clone()).ok())
            .map(|identity| identity == ReplayIdentity::current())
            .unwrap_or(false);
        if !identity_ok {
            absent_reason = "index_identity_mismatch";
            continue;
        }
        let document = match read_index_at(read_root, number) {
            Ok(document) => document,
            Err(reason) => {
                absent_reason = reason;
                continue;
            }
        };
        let generator = document.get("generator").unwrap_or(&Value::Null);
        let kind = generator.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind != "embedding" {
            absent_reason = "semantic_index_lexical_generator";
            continue;
        }
        let key = generator
            .get("generationKey")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if key != wanted {
            absent_reason = "semantic_generation_mismatch";
            continue;
        }
        // Quantization is part of the generation metadata: an int8 generation
        // built by an earlier build has no `quantization` field (or names
        // `int8`) and must not be scored with int4 decoding. Report the
        // generation as mismatched so replay rebuilds a full new generation.
        let quantization = generator.get("quantization").and_then(Value::as_str);
        if quantization != Some(VECTORS_QUANTIZATION) {
            absent_reason = "semantic_generation_mismatch";
            continue;
        }
        return load_document(read_root, number, &document, domain, plan);
    }
    SemanticIndex::degraded(absent_reason)
}

fn load_document(
    read_root: &Path,
    generation: u64,
    document: &Value,
    domain: &str,
    plan: &EmbeddingPlan,
) -> SemanticIndex {
    let key = document
        .get("generator")
        .and_then(|generator| generator.get("generationKey"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let access: Vec<String> = document
        .get("access")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|value| value.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if access.is_empty() || !access.iter().any(|label| label == domain) {
        return SemanticIndex::degraded("semantic_access_restricted");
    }
    let Some(vectors) = document.get("vectors") else {
        return SemanticIndex::degraded("semantic_vectors_missing");
    };
    let dimension = vectors.get("dimension").and_then(Value::as_u64).unwrap_or(0) as usize;
    if dimension != plan.spec.dimension as usize || dimension == 0 {
        return SemanticIndex::degraded("semantic_dimension_mismatch");
    }
    if vectors.get("quantization").and_then(Value::as_str) != Some(VECTORS_QUANTIZATION) {
        return SemanticIndex::degraded("semantic_quantization_unsupported");
    }
    let Some(units_table) = document.get("unitsTable") else {
        return SemanticIndex::degraded("semantic_units_missing");
    };
    let Some(table_path) = units_table.get("path").and_then(Value::as_str) else {
        return SemanticIndex::degraded("semantic_units_missing");
    };
    let Some(table_sha) = units_table.get("sha256").and_then(Value::as_str) else {
        return SemanticIndex::degraded("semantic_units_missing");
    };
    let table_absolute = read_root.join(table_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let table_bytes = match std::fs::read(&table_absolute) {
        Ok(bytes) => bytes,
        Err(_) => return SemanticIndex::degraded("semantic_units_missing"),
    };
    if sha256_hex(&table_bytes) != table_sha {
        return SemanticIndex::degraded("semantic_units_corrupt");
    }
    let unit_entries = match decode_unit_table(&table_bytes) {
        Ok(rows) => rows,
        Err(_) => return SemanticIndex::degraded("semantic_units_corrupt"),
    };
    let count = vectors.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
    if count != unit_entries.len() {
        return SemanticIndex::degraded("semantic_vectors_incomplete");
    }
    let Some(path) = vectors.get("path").and_then(Value::as_str) else {
        return SemanticIndex::degraded("semantic_vectors_missing");
    };
    let Some(expected_sha) = vectors.get("sha256").and_then(Value::as_str) else {
        return SemanticIndex::degraded("semantic_vectors_missing");
    };
    let absolute: PathBuf = read_root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let bytes = match std::fs::read(&absolute) {
        Ok(bytes) => bytes,
        Err(_) => return SemanticIndex::degraded("semantic_vectors_missing"),
    };
    if sha256_hex(&bytes) != expected_sha {
        return SemanticIndex::degraded("semantic_vectors_corrupt");
    }
    let row_bytes = quantized_row_bytes(dimension);
    if bytes.len() != count.saturating_mul(row_bytes) {
        return SemanticIndex::degraded("semantic_vectors_corrupt");
    }
    let mut rows = Vec::with_capacity(count);
    let mut lookup = BTreeMap::new();
    for (index, unit) in unit_entries.iter().enumerate() {
        let Some(row) =
            VectorRow::from_bytes(&bytes[index * row_bytes..(index + 1) * row_bytes], dimension)
        else {
            return SemanticIndex::degraded("semantic_vectors_corrupt");
        };
        rows.push(row);
        lookup.insert(unit.unit_sha256.clone(), index);
    }
    SemanticIndex {
        status: SemanticIndexStatus::Available,
        reason: None,
        generation: Some(generation),
        generation_key: Some(key),
        dimension,
        units: count,
        access,
        rows,
        lookup,
    }
}

/// Reciprocal-rank fusion of one lexical and one semantic candidate list.
///
/// `lexical` / `semantic` are hit ids in rank order. The fused score is
/// `1/(k + lexical_rank) + 1/(k + semantic_rank)` (a missing list contributes 0),
/// which is order-preserving, model-free and explainable.
pub fn rrf_score(lexical_rank: Option<usize>, semantic_rank: Option<usize>) -> f64 {
    let mut score = 0.0;
    if let Some(rank) = lexical_rank {
        score += 1.0 / (RRF_K + (rank + 1) as f64);
    }
    if let Some(rank) = semantic_rank {
        score += 1.0 / (RRF_K + (rank + 1) as f64);
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::semantic::embed::fake_vector;
    use crate::recall::semantic::model::Normalization;

    #[test]
    fn int4_round_trip_keeps_direction() {
        let vector = fake_vector("缓存失效策略", 64, Normalization::L2);
        let bytes = quantize_vector(&vector);
        assert_eq!(bytes.len(), quantized_row_bytes(64));
        let row = VectorRow::from_bytes(&bytes, 64).unwrap();
        let norm = vector_norm(&vector);
        let same = row.similarity(&vector, norm);
        assert!(same > 0.98, "{same}");
        let other = fake_vector("完全不同的内容", 64, Normalization::L2);
        let different = row.similarity(&other, vector_norm(&other));
        assert!(different < same, "{different} vs {same}");
    }

    #[test]
    fn int4_packing_round_trips_signed_boundaries() {
        let vector = vec![10.0, -10.0, 0.0, 5.0, -5.0, 1.0];
        let bytes = quantize_vector(&vector);
        assert_eq!(bytes.len(), quantized_row_bytes(6));
        // Low nibble = even dimension, high nibble = odd dimension; negative
        // values use two's complement (`-7` -> `0x9`, `-4` -> `0xc`).
        assert_eq!(&bytes[4..], &[0x97, 0x40, 0x1c]);
        let row = VectorRow::from_bytes(&bytes, 6).unwrap();
        assert_eq!(row.values, vec![7, -7, 0, 4, -4, 1]);
        let scale = row.scale;
        assert!((row.values[0] as f32 * scale - 10.0).abs() < 1e-4);
        assert!((row.values[1] as f32 * scale + 10.0).abs() < 1e-4);
        let zero = VectorRow::from_bytes(&quantize_vector(&[0.0, 0.0]), 2).unwrap();
        assert_eq!(zero.similarity(&[1.0, 1.0], vector_norm(&[1.0, 1.0])), 0.0);
    }

    #[test]
    fn int4_odd_dimension_pads_the_last_byte() {
        let vector = fake_vector("odd dimension", 5, Normalization::L2);
        let bytes = quantize_vector(&vector);
        assert_eq!(bytes.len(), quantized_row_bytes(5));
        assert_eq!(bytes.len(), VECTOR_SCALE_BYTES + 3);
        assert_eq!(bytes[bytes.len() - 1] & 0xf0, 0);
        let row = VectorRow::from_bytes(&bytes, 5).unwrap();
        assert_eq!(row.values.len(), 5);
        let same = row.similarity(&vector, vector_norm(&vector));
        assert!(same > 0.98, "{same}");
        // Truncated or padded rows are rejected by exact length. Dimensions 5
        // and 6 share a byte count (the odd dimension merely leaves the last
        // high nibble unused); the index dimension decides the row length.
        assert!(VectorRow::from_bytes(&bytes, 4).is_none());
        assert!(VectorRow::from_bytes(&bytes, 7).is_none());
        assert!(VectorRow::from_bytes(&bytes[..bytes.len() - 1], 5).is_none());
    }

    #[test]
    fn rrf_prefers_a_hit_found_by_both_lists() {
        let both = rrf_score(Some(2), Some(4));
        let lexical_only = rrf_score(Some(0), None);
        assert!(both > lexical_only);
        assert!(rrf_score(Some(0), None) > rrf_score(Some(1), None));
        assert_eq!(rrf_score(None, None), 0.0);
    }

    #[test]
    fn binary_unit_table_round_trips_and_rejects_bad_rows() {
        let rows = vec![
            UnitRow {
                unit_sha256: "ab".repeat(32),
                unit_type: "message:user".to_string(),
                source_key: "cd".repeat(16),
                record_sha256: "ef".repeat(32),
                line: Some(3),
                text_bytes: 42,
                turn: 2,
                access: vec!["default".to_string()],
            },
            UnitRow {
                unit_sha256: "12".repeat(32),
                unit_type: "tool_result".to_string(),
                source_key: "34".repeat(16),
                record_sha256: "56".repeat(32),
                line: None,
                text_bytes: 7,
                turn: 0,
                access: vec!["work".to_string(), "default".to_string()],
            },
        ];
        let encoded = encode_unit_table(&rows).unwrap();
        assert_eq!(decode_unit_table(&encoded).unwrap(), rows);
        assert_eq!(encoded, encode_unit_table(&rows).unwrap());

        let mut broken = rows.clone();
        broken[0].unit_sha256 = "not-a-hash".to_string();
        assert!(encode_unit_table(&broken).is_err());
        assert!(decode_unit_table(&encoded[..encoded.len() - 1]).is_err());
    }

    #[test]
    fn quantization_is_deterministic() {
        let vector = fake_vector("determinism", 32, Normalization::L2);
        assert_eq!(quantize_vector(&vector), quantize_vector(&vector));
    }
}
