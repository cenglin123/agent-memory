//! Derived-model replay from the archive (P5).
//!
//! The unified Session/Message model is a **derived artifact**: it can always be
//! rebuilt from the immutable raw objects plus the committed capture manifests.
//! Replay never writes to `archive/` or `state/`; the capture facts stay exactly
//! as the collector recorded them.
//!
//! # Generation checkpoints
//!
//! Derived state lives under `<root>/derived/`:
//!
//! ```text
//! derived/state.json                                  checkpoint index
//! derived/generations/g<N>/sources/<key>.json          per-source artifact
//! ```
//!
//! A checkpoint is keyed by `(parserVersion, modelVersion, archiveSchemaVersion)`
//! and stores, per source, the capture generation / observation sequence, the
//! committed record count, the SHA-256 of the last input record, and the artifact
//! digest. Replay **reuses** an entry only when all of those still match the
//! archive; a changed source is re-parsed, an inconsistent checkpoint is
//! discarded (and reported). A new parser version therefore gets its own
//! generation and never mutates an older generation's artifacts.
//!
//! # Honest limitation
//!
//! Provider parsers are whole-source parsers, so a changed source is re-parsed in
//! full. "Resume" here means unchanged sources are **not re-read or re-parsed**;
//! it is not per-record incremental parsing. Source-level checkpoints are the
//! unit that is actually consistent with the parsers.
//!
//! # Fail-closed reading
//!
//! [`read_derived`] only serves an entry from a generation that is not
//! `rebuilding`, whose artifact hash matches, and whose frozen access labels
//! contain the requesting domain. Empty labels (missing permission mapping) and
//! interrupted rebuilds deny.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::archive::{
    hex_sha256, new_id, now_ms, Archive, ArchiveError, Manifest, ManifestRecord,
    ATTACHMENT_SOURCE_KIND,
};
use crate::capture_sqlite::{SqliteCaptureProfile, SqliteReplay};
use crate::model::SessionMessage;

pub const DERIVED_STATE_SCHEMA: &str = "agent-memory/derived-state@1";
pub const DERIVED_SOURCE_SCHEMA: &str = "agent-memory/derived-source@1";
pub const DERIVED_GENERATION_REBUILDING: &str = "rebuilding";
pub const DERIVED_GENERATION_COMPLETE: &str = "complete";
pub const DERIVED_GENERATION_PARTIAL: &str = "partial";

/// Parser/schema generation identity. A replay built with a different identity
/// must never reuse another identity's checkpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayIdentity {
    pub parser_version: String,
    pub model_version: u32,
    pub archive_schema_version: u32,
}

impl ReplayIdentity {
    /// Default identity: this binary's parser version and the archive schema.
    pub fn current() -> Self {
        Self {
            parser_version: env!("CARGO_PKG_VERSION").to_string(),
            model_version: crate::archive::DERIVED_MODEL_VERSION,
            archive_schema_version: crate::archive::ARCHIVE_SCHEMA_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedSourceEntry {
    pub source: String,
    pub provider: String,
    pub kind: String,
    /// Stable artifact key (first 32 hex of the source SHA-256).
    pub key: String,
    /// Capture generation of the physical source (JSONL/whole-file) ...
    #[serde(default)]
    pub capture_generation: u64,
    /// ... or the observation sequence (SQLite evidence units).
    #[serde(default)]
    pub capture_seq: u64,
    /// `records` or `sqlite_observation`.
    pub input_kind: String,
    /// Records covered by this artifact (JSONL/whole-file).
    #[serde(default)]
    pub records: u64,
    /// SHA-256 of the last covered record (checkpoint boundary).
    #[serde(default)]
    pub record_boundary_sha: String,
    /// Digest of the materialized input object list (whole-file checkpoints).
    #[serde(default)]
    pub input_digest: String,
    /// Frozen reference-level access labels (empty = fail-closed).
    #[serde(default)]
    pub access: Vec<String>,
    pub messages: u64,
    pub sessions: u64,
    pub bytes: u64,
    /// Runtime-root-relative artifact path (`None` for empty/failed sources).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default)]
    pub artifact_sha256: String,
    /// `ok` | `empty` | `parse_failed` | `unsupported`
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub anomalies: Vec<String>,
    pub replayed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedGeneration {
    pub generation: u64,
    pub identity: ReplayIdentity,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// `complete` | `partial` | `rebuilding`
    pub status: String,
    #[serde(default)]
    pub sources: Vec<DerivedSourceEntry>,
    #[serde(default)]
    pub anomalies: Vec<String>,
    /// P1 retrieval skeleton (`derived/generations/g<N>/retrieval/`). This is a
    /// rebuildable artifact, never evidence: deleting it changes nothing except
    /// the derived layer, and a replay reproduces the same bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<RetrievalIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedState {
    pub schema: String,
    #[serde(default)]
    pub generations: Vec<DerivedGeneration>,
}

// ---------------------------------------------------------------------------
// Retrieval index skeleton (P1)
// ---------------------------------------------------------------------------
//
// The retrieval artifact lives under the *derived* generation, never beside the
// evidence: it carries the parser generation, the embedding identity (model id,
// dimension, normalization, backend) and the frozen access labels, so a model
// change forces a new generation instead of silently mixing vectors. P1 writes
// the skeleton only: unit identities and their derived location, no vectors and
// no event storage.

pub use crate::recall::semantic::index::{RETRIEVAL_INDEX_SCHEMA, RETRIEVAL_UNIT_PARSER};
pub const RETRIEVAL_GENERATOR_LEXICAL: &str = "lexical-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalGenerator {
    /// `lexical` (no embedding configured) or `embedding`.
    pub kind: String,
    /// `lexical-v1` or the frozen `embed-v1;model=…;dim=…;norm=…;backend=…`.
    pub generation_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Vector storage format of this generation (`int4`). Part of the
    /// generation metadata: changing it forces a full rebuild instead of mixing
    /// encodings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
}

impl RetrievalGenerator {
    pub fn lexical() -> Self {
        RetrievalGenerator {
            kind: "lexical".to_string(),
            generation_key: RETRIEVAL_GENERATOR_LEXICAL.to_string(),
            model_id: None,
            dimension: None,
            normalization: None,
            backend: None,
            quantization: None,
        }
    }

    pub fn embedding(spec: &crate::recall::semantic::model::EmbeddingModelSpec) -> Self {
        RetrievalGenerator {
            kind: "embedding".to_string(),
            generation_key: spec.generation_key(),
            model_id: Some(spec.model_id.clone()),
            dimension: Some(spec.dimension),
            normalization: Some(spec.normalization.as_str().to_string()),
            backend: Some(spec.backend.as_str().to_string()),
            quantization: Some(
                crate::recall::semantic::index::VECTORS_QUANTIZATION.to_string(),
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalIndexEntry {
    /// Runtime-root-relative artifact path.
    pub path: String,
    pub sha256: String,
    pub units: u64,
    pub generation_key: String,
    /// `mapped` when every indexed source has a derived session id.
    pub session_mapping: String,
    /// P3 vector artifact of the same generation. `None` for the lexical
    /// generator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors: Option<RetrievalVectorEntry>,
    /// Binary unit table of the same generation (fixed-width hashes + small
    /// integer ids; keeps the size budget under short sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units_table: Option<RetrievalUnitEntry>,
    /// Relevance threshold generation used by search for this index.
    #[serde(default)]
    pub thresholds_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalUnitEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub count: u64,
    pub schema: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalVectorEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub dimension: u64,
    /// `int4` scalar quantization (two signed 4-bit values per byte, plus one
    /// f32 scale per unit: `4 + ceil(dimension / 2)` bytes).
    pub quantization: String,
    pub count: u64,
}

/// Outcome of the semantic half of a replay, reported next to the lexical
/// skeleton. A failure here is always a degradation: the lexical index is rebuilt
/// and search keeps working.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaySemanticStatus {
    /// `available` | `degraded` | `disabled`
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub backend: String,
    pub generation_key: String,
    pub dimension: u64,
    pub units: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrievalUnit {
    unit_sha256: String,
    unit_type: String,
    /// Artifact key of the source (first 32 hex of its path hash); `sources[]`
    /// maps it to the physical source and derived artifact.
    source_key: String,
    /// Committed record this unit was projected from (the readback anchor).
    record_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    text_bytes: u64,
    /// 1-based conversational turn (user message + following records); 0 for
    /// records before the first user message.
    turn: u32,
    access: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrievalSource {
    source: String,
    artifact: String,
    units: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    /// `mapped` | `unmapped`
    session_mapping: String,
    access: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrievalVectors {
    path: String,
    sha256: String,
    bytes: u64,
    dimension: u64,
    quantization: &'static str,
    count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrievalUnitsTable {
    path: String,
    sha256: String,
    bytes: u64,
    count: u64,
    schema: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrievalIndex {
    schema: &'static str,
    identity: ReplayIdentity,
    generator: RetrievalGenerator,
    unit_parser: &'static str,
    /// Relevance threshold generation that produced/accepts this index.
    thresholds_version: &'static str,
    /// Fusion identity when the semantic path is configured
    /// (`semantic_cosine_lexical_bonus`); absent for the lexical-only generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    score_fusion: Option<&'static str>,
    /// Bounded lexical weight used by the semantic-dominant fusion; absent for
    /// the lexical-only generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    lexical_bonus: Option<f64>,
    /// Strict intersection of every indexed source's access labels (empty =
    /// fail-closed).
    access: Vec<String>,
    sources: Vec<RetrievalSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vectors: Option<RetrievalVectors>,
    #[serde(rename = "unitsTable")]
    units_table: RetrievalUnitsTable,
    unit_count: u64,
    /// Total normalized unit text bytes (size-budget denominator).
    unit_text_bytes: u64,
}

fn retrieval_rel(generation: u64) -> String {
    crate::recall::semantic::index::retrieval_index_rel(generation)
}

/// Build (or rebuild) the retrieval index of one derived generation.
///
/// Units are projected from the **committed records** (not from the decoded
/// message model), so every vector keeps the exact record identity that
/// `memory_read` resolves. The document is a pure function of the archive, the
/// identity, the generator and the embedding plan: no timestamps, no generation
/// number, no capture ids. Two builds over the same inputs therefore produce
/// identical bytes, including after the whole `derived/` tree was deleted.
fn build_retrieval_index(
    archive: &Archive,
    generation: u64,
    domain: &str,
    entries: &[DerivedSourceEntry],
    identity: &ReplayIdentity,
    generator: &RetrievalGenerator,
    embedding: Option<&crate::recall::semantic::embed::EmbeddingPlan>,
) -> Result<(RetrievalIndexEntry, Option<ReplaySemanticStatus>), ArchiveError> {
    let embedding = match (generator.kind.as_str(), embedding) {
        ("embedding", Some(plan)) => {
            if plan.generation_key() != generator.generation_key {
                return Err(ArchiveError::Embedding(format!(
                    "embedding plan {} does not match generator {}",
                    plan.generation_key(),
                    generator.generation_key
                )));
            }
            Some(plan)
        }
        ("embedding", None) => {
            return Err(ArchiveError::Embedding(
                "embedding_plan_missing".to_string(),
            ))
        }
        _ => None,
    };

    let mut ordered: Vec<&DerivedSourceEntry> = entries.iter().collect();
    ordered.sort_by(|a, b| a.source.cmp(&b.source));

    let mut sources: Vec<RetrievalSource> = Vec::new();
    let mut access_lists: Vec<&Vec<String>> = Vec::new();
    let mut by_source: BTreeMap<String, Option<String>> = BTreeMap::new();

    for entry in &ordered {
        let Some(artifact) = entry.artifact.as_deref() else {
            continue;
        };
        if entry.status != "ok" {
            continue;
        }
        let path = artifact_abs(archive, artifact);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        let document: Value = match serde_json::from_slice(&raw) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let sessions = document
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let session_id = if sessions.len() == 1 {
            sessions[0]
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        } else {
            None
        };
        by_source.insert(entry.source.clone(), session_id.clone());
        sources.push(RetrievalSource {
            source: entry.source.clone(),
            artifact: artifact.to_string(),
            units: 0,
            session_id: session_id.clone(),
            session_mapping: if session_id.is_some() {
                "mapped".to_string()
            } else {
                "unmapped".to_string()
            },
            access: entry.access.clone(),
        });
        access_lists.push(&entry.access);
    }

    // Evidence units in archive order (the same selection as the Archive
    // provider), projected per provider into message / tool-call / tool-result
    // units. Deduplication is per physical source: two sources that captured the
    // same content keep their own vectors (§3.5 "cross-source vectors are never
    // shared").
    let manifests = archive.manifests()?;
    let evidence = crate::recall::providers::archive::load_units(&manifests);
    let mut units: Vec<RetrievalUnit> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut object_cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut turns: BTreeMap<String, u32> = BTreeMap::new();
    for unit in &evidence {
        if !by_source.contains_key(&unit.source) {
            continue;
        };
        if !object_cache.contains_key(&unit.record.object) {
            let bytes = archive.reader(domain).read_object(&unit.record.object)?;
            object_cache.insert(unit.record.object.clone(), bytes);
        }
        let bytes = object_cache
            .get(&unit.record.object)
            .expect("object cached above");
        let start = unit.record.offset as usize;
        let end = start.saturating_add(unit.record.len as usize);
        let Some(slice) = bytes.get(start..end) else {
            continue;
        };
        if hex_sha256(slice) != unit.record.sha256 {
            continue;
        }
        let Some(projection) = crate::recall::providers::archive::project_record(slice) else {
            continue;
        };
        // Turn index is per physical source and mirrors the evaluation contract:
        // a user message opens a turn, records before the first user message are
        // turn 0, and metadata never opens one.
        let turn = {
            let current = turns.entry(unit.source.clone()).or_insert(0);
            if crate::recall::semantic::units::is_user_turn_record(&unit.provider, &projection) {
                *current += 1;
            }
            *current
        };
        for piece in crate::recall::semantic::units::extract(&unit.provider, &projection) {
            let text = crate::recall::semantic::units::normalize_unit_text(&piece.text);
            if text.is_empty() {
                continue;
            }
            let unit_sha =
                crate::recall::semantic::units::unit_sha256(&text, &piece.unit_type);
            if !seen.insert((unit.source.clone(), unit_sha.clone())) {
                continue;
            }
            units.push(RetrievalUnit {
                unit_sha256: unit_sha,
                unit_type: piece.unit_type,
                source_key: source_key(&unit.source),
                record_sha256: unit.record.sha256.clone(),
                line: unit.record.source_line,
                text_bytes: text.len() as u64,
                turn,
                access: unit.record.access.clone(),
            });
            texts.push(text);
        }
    }

    let vectors = match embedding {
        Some(plan) if !texts.is_empty() => {
            let vectors = plan.embed_batch(&texts).map_err(|error| {
                ArchiveError::Embedding(format!(
                    "{}: {}",
                    error.reason,
                    error.detail.unwrap_or_default()
                ))
            })?;
            if vectors.len() != texts.len() {
                return Err(ArchiveError::Embedding(format!(
                    "embedding returned {} vectors for {} units",
                    vectors.len(),
                    texts.len()
                )));
            }
            let mut blob: Vec<u8> = Vec::new();
            for vector in &vectors {
                blob.extend_from_slice(&crate::recall::semantic::index::quantize_vector(vector));
            }
            let relative = crate::recall::semantic::index::vectors_rel(generation);
            let path = artifact_abs(archive, &relative);
            crate::config::atomic_write(&path, &blob)?;
            Some(RetrievalVectors {
                path: relative,
                sha256: hex_sha256(&blob),
                bytes: blob.len() as u64,
                dimension: plan.spec.dimension,
                quantization: crate::recall::semantic::index::VECTORS_QUANTIZATION,
                count: vectors.len() as u64,
            })
        }
        _ => None,
    };

    for source in &mut sources {
        let key = source_key(&source.source);
        source.units = units
            .iter()
            .filter(|unit| unit.source_key == key)
            .count() as u64;
    }

    let rows: Vec<crate::recall::semantic::index::UnitRow> = units
        .iter()
        .map(|unit| crate::recall::semantic::index::UnitRow {
            unit_sha256: unit.unit_sha256.clone(),
            unit_type: unit.unit_type.clone(),
            source_key: unit.source_key.clone(),
            record_sha256: unit.record_sha256.clone(),
            line: unit.line,
            text_bytes: unit.text_bytes,
            turn: unit.turn,
            access: unit.access.clone(),
        })
        .collect();
    let table_bytes = crate::recall::semantic::index::encode_unit_table(&rows)
        .map_err(ArchiveError::Corrupt)?;
    let table_relative = crate::recall::semantic::index::units_rel(generation);
    crate::config::atomic_write(
        &artifact_abs(archive, &table_relative),
        &table_bytes,
    )?;
    let units_table = RetrievalUnitsTable {
        path: table_relative,
        sha256: hex_sha256(&table_bytes),
        bytes: table_bytes.len() as u64,
        count: units.len() as u64,
        schema: crate::recall::semantic::index::UNITS_SCHEMA,
    };

    let access = intersection(&access_lists);
    let session_mapping = if sources
        .iter()
        .all(|source| source.session_mapping == "mapped")
    {
        "mapped"
    } else {
        "unmapped"
    };
    let document = RetrievalIndex {
        schema: RETRIEVAL_INDEX_SCHEMA,
        identity: identity.clone(),
        generator: generator.clone(),
        unit_parser: RETRIEVAL_UNIT_PARSER,
        thresholds_version: crate::recall::semantic::relevance::ARCHIVE_RELEVANCE_VERSION,
        score_fusion: (generator.kind == "embedding")
            .then_some(crate::recall::semantic::relevance::SCORE_FUSION),
        lexical_bonus: (generator.kind == "embedding")
            .then_some(crate::recall::semantic::relevance::LEXICAL_BONUS),
        access,
        sources,
        vectors: vectors.clone(),
        units_table: units_table.clone(),
        unit_count: units.len() as u64,
        unit_text_bytes: rows.iter().map(|row| row.text_bytes).sum(),
    };
    let path = archive.root().join(retrieval_rel(generation));
    // Compact JSON: the index is a machine artifact, and the size budget counts
    // its bytes against the indexed source text.
    let data = serde_json::to_vec(&document)
        .map_err(|error| ArchiveError::Corrupt(error.to_string()))?;
    crate::config::atomic_write(&path, &data)?;
    let semantic = embedding.map(|plan| ReplaySemanticStatus {
        status: "available".to_string(),
        reason: None,
        backend: plan.backend().to_string(),
        generation_key: plan.generation_key(),
        dimension: plan.spec.dimension,
        units: document.unit_count,
        detail: None,
    });
    let entry_units = RetrievalUnitEntry {
        path: units_table.path.clone(),
        sha256: units_table.sha256.clone(),
        bytes: units_table.bytes,
        count: units_table.count,
        schema: units_table.schema.to_string(),
    };
    let entry_vectors = vectors.as_ref().map(|vectors| RetrievalVectorEntry {
        path: vectors.path.clone(),
        sha256: vectors.sha256.clone(),
        bytes: vectors.bytes,
        dimension: vectors.dimension,
        quantization: vectors.quantization.to_string(),
        count: vectors.count,
    });
    Ok((
        RetrievalIndexEntry {
            path: retrieval_rel(generation),
            sha256: hex_sha256(&data),
            units: document.unit_count,
            generation_key: generator.generation_key.clone(),
            session_mapping: session_mapping.to_string(),
            vectors: entry_vectors,
            units_table: Some(entry_units),
            thresholds_version:
                crate::recall::semantic::relevance::ARCHIVE_RELEVANCE_VERSION.to_string(),
        },
        semantic,
    ))
}

impl Default for DerivedState {
    fn default() -> Self {
        Self {
            schema: DERIVED_STATE_SCHEMA.to_string(),
            generations: Vec::new(),
        }
    }
}

fn state_path(archive: &Archive) -> PathBuf {
    archive.root().join("derived").join("state.json")
}

pub fn load_state(archive: &Archive) -> Result<DerivedState, ArchiveError> {
    let path = state_path(archive);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DerivedState::default()),
        Err(error) => return Err(ArchiveError::Io(crate::error::AppError::io(&path, error))),
    };
    let state: DerivedState = serde_json::from_slice(&raw)
        .map_err(|error| ArchiveError::Corrupt(format!("derived/state.json: {error}")))?;
    if state.schema != DERIVED_STATE_SCHEMA {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported derived state schema {}",
            state.schema
        )));
    }
    Ok(state)
}

fn save_state(archive: &Archive, state: &DerivedState) -> Result<(), ArchiveError> {
    let path = state_path(archive);
    let data = serde_json::to_string_pretty(state)
        .map_err(|error| ArchiveError::Corrupt(error.to_string()))?;
    crate::config::atomic_write(&path, data.as_bytes())?;
    Ok(())
}

/// What coverage needs to know about the current parser generation.
#[derive(Debug, Clone)]
pub struct CheckpointSnapshot {
    pub generation: Option<u64>,
    pub status: String,
    /// Latest entry per source path.
    pub entries: BTreeMap<String, DerivedSourceEntry>,
    pub anomalies: Vec<String>,
}

pub fn checkpoint_snapshot(archive: &Archive) -> Result<CheckpointSnapshot, ArchiveError> {
    let identity = ReplayIdentity::current();
    checkpoint_snapshot_for(archive, &identity)
}

pub fn checkpoint_snapshot_for(
    archive: &Archive,
    identity: &ReplayIdentity,
) -> Result<CheckpointSnapshot, ArchiveError> {
    let state = load_state(archive)?;
    let Some(generation) = state
        .generations
        .iter()
        .filter(|generation| &generation.identity == identity)
        .max_by_key(|generation| generation.generation)
    else {
        return Ok(CheckpointSnapshot {
            generation: None,
            status: "absent".to_string(),
            entries: BTreeMap::new(),
            anomalies: Vec::new(),
        });
    };
    let mut entries = BTreeMap::new();
    for entry in &generation.sources {
        entries.insert(entry.source.clone(), entry.clone());
    }
    let mut anomalies = generation.anomalies.clone();
    // Cheap consistency check: an `ok` checkpoint whose artifact disappeared is
    // not usable. Hash verification stays in `read_derived` (avoid reading every
    // artifact on every coverage call).
    for entry in entries.values_mut() {
        if entry.status == "ok" {
            let present = entry
                .artifact
                .as_deref()
                .map(|relative| artifact_abs(archive, relative).exists())
                .unwrap_or(false);
            if !present {
                anomalies.push(format!(
                    "derived artifact for {} is missing; checkpoint is inconsistent",
                    entry.source
                ));
                entry.status = "inconsistent".to_string();
            }
        }
    }
    if generation.status == DERIVED_GENERATION_REBUILDING {
        anomalies.push(format!(
            "derived generation {} was interrupted mid-rebuild; derived reads are denied until a replay completes",
            generation.generation
        ));
    }
    Ok(CheckpointSnapshot {
        generation: Some(generation.generation),
        status: generation.status.clone(),
        entries,
        anomalies,
    })
}

// ---------------------------------------------------------------------------
// Replay input inventory
// ---------------------------------------------------------------------------

struct SourceInput {
    source: String,
    provider: String,
    kind: String,
    generation: u64,
    capture_seq: u64,
    input_kind: String,
    records: Vec<ManifestRecord>,
    /// Bytes that must be materialized for the parser (JSONL/whole-file).
    stream_objects: Vec<(u64, String, String)>,
    /// Digest of the stream object list (whole-file checkpoints need it: such a
    /// capture has no per-record boundary).
    input_digest: String,
    access: Vec<String>,
}

fn stream_digest(objects: &[(u64, String, String)]) -> String {
    let mut canonical = Vec::new();
    for (offset, sha, role) in objects {
        canonical.extend_from_slice(offset.to_string().as_bytes());
        canonical.push(b':');
        canonical.extend_from_slice(sha.as_bytes());
        canonical.push(b':');
        canonical.extend_from_slice(role.as_bytes());
        canonical.push(b'\n');
    }
    hex_sha256(&canonical)
}

fn intersection(lists: &[&Vec<String>]) -> Vec<String> {
    let mut iter = lists.iter();
    let Some(first) = iter.next() else {
        return Vec::new();
    };
    let mut out: BTreeSet<String> = first.iter().cloned().collect();
    for list in iter {
        let other: BTreeSet<String> = list.iter().cloned().collect();
        out = out.intersection(&other).cloned().collect();
    }
    out.into_iter().collect()
}

fn collect_inputs(archive: &Archive) -> Result<Vec<SourceInput>, ArchiveError> {
    let manifests = archive.manifests()?;
    let mut by_source: BTreeMap<String, Vec<&Manifest>> = BTreeMap::new();
    for manifest in manifests.iter() {
        if manifest.source.kind == ATTACHMENT_SOURCE_KIND {
            continue;
        }
        by_source
            .entry(manifest.source.path.clone())
            .or_default()
            .push(manifest);
    }
    let mut out = Vec::new();
    for (source, group) in by_source {
        let last = group.last().expect("non-empty group");
        let provider = last.provider.clone();
        let kind = last.source.kind.clone();
        if kind == "sqlite" {
            let sqlite = last.source.sqlite.as_ref();
            let mut access_lists: Vec<&Vec<String>> = Vec::new();
            for manifest in &group {
                for object in &manifest.objects {
                    access_lists.push(&object.access);
                }
            }
            let access = intersection(&access_lists);
            out.push(SourceInput {
                source,
                provider,
                kind,
                generation: last.source.generation,
                capture_seq: sqlite.map(|sqlite| sqlite.capture_seq).unwrap_or(0),
                input_kind: "sqlite_observation".to_string(),
                records: Vec::new(),
                stream_objects: Vec::new(),
                input_digest: String::new(),
                access,
            });
            continue;
        }
        if kind == "whole_file" || kind == "compressed_jsonl_file" {
            // Every full capture is a complete replacement; the latest manifest
            // is the current evidence for this physical source.
            let role = if kind == "compressed_jsonl_file" {
                "decoded"
            } else {
                "whole_file"
            };
            let object = last
                .objects
                .iter()
                .find(|object| object.role == role)
                .or_else(|| last.objects.first());
            let mut stream_objects = Vec::new();
            let mut access_lists: Vec<&Vec<String>> = Vec::new();
            if let Some(object) = object {
                stream_objects.push((
                    object.source_offset.unwrap_or(0),
                    object.sha256.clone(),
                    object.role.clone(),
                ));
            }
            for object in &last.objects {
                access_lists.push(&object.access);
            }
            let input_digest = stream_digest(&stream_objects);
            out.push(SourceInput {
                source,
                provider,
                kind,
                generation: last.source.generation.max(1),
                capture_seq: 0,
                input_kind: "records".to_string(),
                records: last.records.clone(),
                stream_objects,
                input_digest,
                access: intersection(&access_lists),
            });
            continue;
        }

        // JSONL incremental: the maximum capture generation is the current
        // evidence generation. Older generations stay readable but are not the
        // input of this replay.
        let max_generation = group
            .iter()
            .map(|manifest| manifest.source.generation)
            .max()
            .unwrap_or(1);
        let mut records: Vec<ManifestRecord> = Vec::new();
        let mut stream_objects: Vec<(u64, String, String)> = Vec::new();
        let mut access_lists: Vec<&Vec<String>> = Vec::new();
        for manifest in &group {
            if manifest.source.generation != max_generation {
                continue;
            }
            records.extend(manifest.records.iter().cloned());
            for object in &manifest.objects {
                if object.role == "whole_file" || object.role == "append_range" {
                    stream_objects.push((
                        object.source_offset.unwrap_or(manifest.source.range_start),
                        object.sha256.clone(),
                        object.role.clone(),
                    ));
                }
            }
            for record in &manifest.records {
                access_lists.push(&record.access);
            }
        }
        stream_objects.sort_by_key(|(offset, _, _)| *offset);
        stream_objects.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        let input_digest = stream_digest(&stream_objects);
        out.push(SourceInput {
            source,
            provider,
            kind,
            generation: max_generation,
            capture_seq: 0,
            input_kind: "records".to_string(),
            records,
            stream_objects,
            input_digest,
            access: intersection(&access_lists),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DerivedSession {
    session_id: String,
    messages: Vec<SessionMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DerivedArtifact {
    schema: String,
    source: String,
    provider: String,
    kind: String,
    identity: ReplayIdentity,
    capture_generation: u64,
    capture_seq: u64,
    input_kind: String,
    records: u64,
    record_boundary_sha: String,
    input_digest: String,
    access: Vec<String>,
    sessions: Vec<DerivedSession>,
}

pub fn source_key(source: &str) -> String {
    hex_sha256(source.as_bytes())[..32].to_string()
}

fn artifact_rel(generation: u64, key: &str) -> String {
    format!("derived/generations/g{generation}/sources/{key}.json")
}

fn artifact_abs(archive: &Archive, relative: &str) -> PathBuf {
    archive.root().join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

fn artifact_digest(archive: &Archive, relative: &str) -> Option<String> {
    let path = artifact_abs(archive, relative);
    fs::read(path).ok().map(|bytes| hex_sha256(&bytes))
}

fn write_artifact(
    archive: &Archive,
    generation: u64,
    input: &SourceInput,
    identity: &ReplayIdentity,
    sessions: Vec<DerivedSession>,
) -> Result<(String, String, u64), ArchiveError> {
    let key = source_key(&input.source);
    let relative = artifact_rel(generation, &key);
    let path = artifact_abs(archive, &relative);
    let artifact = DerivedArtifact {
        schema: DERIVED_SOURCE_SCHEMA.to_string(),
        source: input.source.clone(),
        provider: input.provider.clone(),
        kind: input.kind.clone(),
        identity: identity.clone(),
        capture_generation: input.generation,
        capture_seq: input.capture_seq,
        input_kind: input.input_kind.clone(),
        records: input.records.len() as u64,
        record_boundary_sha: input
            .records
            .last()
            .map(|record| record.sha256.clone())
            .unwrap_or_default(),
        input_digest: input.input_digest.clone(),
        access: input.access.clone(),
        sessions,
    };
    let data = serde_json::to_vec_pretty(&artifact)
        .map_err(|error| ArchiveError::Corrupt(error.to_string()))?;
    crate::config::atomic_write(&path, &data)?;
    Ok((relative, hex_sha256(&data), data.len() as u64))
}

/// Read one artifact through its reference handle.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedReadMeta {
    pub derived_ref: String,
    pub generation: u64,
    pub source: String,
    pub parser_version: String,
    pub model_version: u32,
    pub sha256: String,
    pub bytes: u64,
    pub access: Vec<String>,
    pub verified: bool,
}

pub fn read_derived(
    archive: &Archive,
    derived_ref: &str,
    domain: &str,
) -> Result<(Vec<u8>, DerivedReadMeta), ArchiveError> {
    if domain.is_empty() {
        return Err(ArchiveError::AccessDenied(
            "empty access domain (fail-closed)".to_string(),
        ));
    }
    let rest = derived_ref
        .strip_prefix("derived:")
        .ok_or_else(|| ArchiveError::NotFound(format!("derived ref {derived_ref}")))?;
    let (generation_raw, key) = rest
        .split_once(':')
        .ok_or_else(|| ArchiveError::NotFound(format!("derived ref {derived_ref}")))?;
    let generation_number: u64 = generation_raw
        .parse()
        .map_err(|_| ArchiveError::NotFound(format!("derived ref {derived_ref}")))?;
    if key.is_empty() {
        return Err(ArchiveError::NotFound(format!("derived ref {derived_ref}")));
    }
    let state = load_state(archive)?;
    let generation = state
        .generations
        .iter()
        .find(|generation| generation.generation == generation_number)
        .ok_or_else(|| ArchiveError::NotFound(format!("derived generation {generation_number}")))?;
    if generation.status == DERIVED_GENERATION_REBUILDING {
        return Err(ArchiveError::AccessDenied(format!(
            "derived generation {generation_number} is rebuilding (fail-closed)"
        )));
    }
    let entry = generation
        .sources
        .iter()
        .find(|entry| entry.key == key && entry.status == "ok")
        .ok_or_else(|| {
            ArchiveError::NotFound(format!(
                "derived entry {key} in generation {generation_number} is not readable"
            ))
        })?;
    if entry.access.is_empty() {
        return Err(ArchiveError::AccessDenied(format!(
            "derived entry {key} has no access mapping (fail-closed)"
        )));
    }
    if !entry.access.iter().any(|value| value == domain) {
        return Err(ArchiveError::AccessDenied(format!(
            "derived entry {key} is not visible to domain {domain:?}"
        )));
    }
    let relative = entry
        .artifact
        .as_deref()
        .ok_or_else(|| ArchiveError::NotFound(format!("derived entry {key} has no artifact")))?;
    let bytes = fs::read(artifact_abs(archive, relative)).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ArchiveError::NotFound(format!("derived artifact {relative}"))
        } else {
            ArchiveError::Io(crate::error::AppError::io(
                &artifact_abs(archive, relative),
                error,
            ))
        }
    })?;
    let actual = hex_sha256(&bytes);
    if actual != entry.artifact_sha256 {
        return Err(ArchiveError::Integrity(format!(
            "derived artifact {relative} hashes to {actual}, checkpoint says {}",
            entry.artifact_sha256
        )));
    }
    let meta = DerivedReadMeta {
        derived_ref: format!("derived:{}:{}", generation_number, key),
        generation: generation_number,
        source: entry.source.clone(),
        parser_version: generation.identity.parser_version.clone(),
        model_version: generation.identity.model_version,
        sha256: actual,
        bytes: bytes.len() as u64,
        access: entry.access.clone(),
        verified: true,
    };
    Ok((bytes, meta))
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaySourceReport {
    pub source: String,
    pub provider: String,
    pub kind: String,
    /// replayed | reused | empty | parse_failed | unsupported
    pub status: String,
    pub input_kind: String,
    pub records: u64,
    pub records_replayed: u64,
    pub capture_generation: u64,
    pub capture_seq: u64,
    pub messages: u64,
    pub sessions: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_ref: Option<String>,
    pub checkpoint_reused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub anomalies: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayReport {
    pub ok: bool,
    /// `resumed` (same parser/model generation) or `new_generation`.
    pub mode: String,
    pub generation: u64,
    pub identity: ReplayIdentity,
    pub sources: Vec<ReplaySourceReport>,
    pub reused_sources: u64,
    pub replayed_sources: u64,
    pub failed_sources: u64,
    pub messages: u64,
    pub manifest_digest_before: String,
    pub manifest_digest_after: String,
    pub derived_state_path: String,
    /// P1/P3 retrieval artifact built for this generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<RetrievalIndexEntry>,
    /// P3 semantic half: available, degraded (explicit reason) or disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<ReplaySemanticStatus>,
    #[serde(default)]
    pub anomalies: Vec<String>,
}

/// Scratch directory for materialized sources. It lives under `derived/tmp/`
/// (never inside `archive/`) and is removed when replay ends.
struct Prepared {
    temp: PathBuf,
}

impl Prepared {
    fn new(archive: &Archive) -> Result<Self, ArchiveError> {
        let base = archive.root().join("derived").join("tmp");
        fs::create_dir_all(&base).map_err(|error| crate::error::AppError::io(&base, error))?;
        let temp = base.join(new_id("replay"));
        fs::create_dir_all(&temp).map_err(|error| crate::error::AppError::io(&temp, error))?;
        Ok(Self { temp })
    }

    fn path(&self, key: &str, extension: &str) -> PathBuf {
        self.temp.join(format!("{key}.{extension}"))
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp);
    }
}

fn manifest_digest(manifests: &[Manifest]) -> String {
    // Full committed-manifest bytes, not a selected projection: replay must not
    // change any capture fact (including access tags, locators and attachments).
    let bytes = serde_json::to_vec(manifests).unwrap_or_default();
    hex_sha256(&bytes)
}

fn parse_stream(
    provider: &str,
    path: &Path,
    source: &str,
) -> Result<Vec<DerivedSession>, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "materialized path is not valid UTF-8".to_string())?;
    let messages = crate::load_messages(provider, text)?;
    let session_id = session_id_from_source(source);
    Ok(vec![DerivedSession {
        session_id,
        messages,
    }])
}

fn session_id_from_source(source: &str) -> String {
    let name = Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| source.to_string());
    name.trim_end_matches(".jsonl")
        .trim_end_matches(".jsonl.zst")
        .trim_end_matches(".json")
        .to_string()
}

fn replay_records(
    archive: &Archive,
    input: &SourceInput,
    domain: &str,
    prepared: &Prepared,
) -> Result<Vec<DerivedSession>, ArchiveError> {
    let key = source_key(&input.source);
    let mut bytes = Vec::new();
    for (_, sha, _) in &input.stream_objects {
        let chunk = archive.reader(domain).read_object(sha)?;
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() && !input.records.is_empty() {
        return Err(ArchiveError::Integrity(format!(
            "source {} has {} committed records but no object bytes",
            input.source,
            input.records.len()
        )));
    }
    let extension = if input.kind == "compressed_jsonl_file" {
        "jsonl"
    } else {
        Path::new(&input.source)
            .extension()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("json")
    };
    let path = prepared.path(&key, extension);
    fs::write(&path, &bytes)
        .map_err(|error| ArchiveError::Io(crate::error::AppError::io(&path, error)))?;
    parse_stream(&input.provider, &path, &input.source)
        .map_err(|error| ArchiveError::Corrupt(format!("{}: {error}", input.source)))
}

// -- SQLite observation materialization --------------------------------------

fn sql_value(value: &Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(flag) => rusqlite::types::Value::Integer(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                rusqlite::types::Value::Integer(integer)
            } else if let Some(float) = number.as_f64() {
                rusqlite::types::Value::Real(float)
            } else {
                rusqlite::types::Value::Null
            }
        }
        Value::String(text) => rusqlite::types::Value::Text(text.clone()),
        Value::Array(_) => rusqlite::types::Value::Text(value.to_string()),
        Value::Object(map) => {
            if let Some(hex) = map.get("blob_hex").and_then(Value::as_str) {
                match decode_hex(hex) {
                    Some(bytes) => rusqlite::types::Value::Blob(bytes),
                    None => rusqlite::types::Value::Null,
                }
            } else if let Some(hex) = map.get("invalid_utf8_hex").and_then(Value::as_str) {
                match decode_hex(hex) {
                    Some(bytes) => rusqlite::types::Value::Blob(bytes),
                    None => rusqlite::types::Value::Null,
                }
            } else {
                rusqlite::types::Value::Text(value.to_string())
            }
        }
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push(((high << 4) | low) as u8);
    }
    Some(out)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Materialize a readable SQLite database at one observation point.
///
/// The baseline's `CREATE TABLE` statements supply the schema (they are stored
/// in the committed manifest); the replayed row state supplies the row
/// contents. Inserts run with `PRAGMA foreign_keys = OFF` and in parent-first
/// table order, so a child row that appears before its parent in the delta
/// stream cannot fail the rebuild; `PRAGMA foreign_key_check` afterwards
/// reports (never fails) any residual violation. This is the observation-point
/// state, not a byte-level reconstruction of the live file (P3 never claims the
/// latter).
/// Pre-existing tests pin the OpenCode profile ordering; production resolves
/// the profile from the committed baseline evidence.
#[cfg(test)]
fn materialize_observation(
    replay: &SqliteReplay,
    ddl: &BTreeMap<String, String>,
    prepared: &Prepared,
    key: &str,
) -> Result<(PathBuf, Vec<String>), String> {
    materialize_observation_in(&SqliteCaptureProfile::OPENCODE, replay, ddl, prepared, key)
}

fn materialize_observation_in(
    profile: &SqliteCaptureProfile,
    replay: &SqliteReplay,
    ddl: &BTreeMap<String, String>,
    prepared: &Prepared,
    key: &str,
) -> Result<(PathBuf, Vec<String>), String> {
    if ddl.is_empty() {
        return Err("baseline manifest exposes no table schema".to_string());
    }
    let out = prepared.path(key, "db");
    if out.exists() {
        let _ = fs::remove_file(&out);
    }
    let conn = rusqlite::Connection::open(&out).map_err(|error| format!("open output: {error}"))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .map_err(|error| format!("disable foreign keys: {error}"))?;
    // Parent tables first (profile replay order: OpenCode materializes project
    // → session/session_v2 → message/session_message → part/todo), then any
    // remaining tables by name.
    let ordered_tables: Vec<&String> = {
        let mut tables: Vec<&String> = replay.tables.keys().collect();
        tables.sort_by_key(|table| {
            profile
                .replay_order
                .iter()
                .position(|known| known == table)
                .unwrap_or(usize::MAX)
        });
        tables
    };
    for table in &ordered_tables {
        let Some(create) = ddl.get(*table) else {
            continue;
        };
        conn.execute_batch(create)
            .map_err(|error| format!("create {table}: {error}"))?;
    }
    for (name, sql) in ddl {
        if replay.tables.contains_key(name) {
            continue;
        }
        // Schema without archived rows (e.g. an empty table) still materializes.
        let _ = conn.execute_batch(sql);
    }
    for table in &ordered_tables {
        let rows = match replay.tables.get(*table) {
            Some(rows) => rows,
            None => continue,
        };
        let columns: Vec<String> = replay
            .columns
            .get(*table)
            .cloned()
            .unwrap_or_default();
        for row in rows.values() {
            let Some(object) = row.as_object() else { continue };
            let columns: Vec<String> = if columns.is_empty() {
                object.keys().cloned().collect()
            } else {
                columns.clone()
            };
            let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "INSERT OR REPLACE INTO {} ({}) VALUES ({})",
                quote_ident(table),
                columns
                    .iter()
                    .map(|column| quote_ident(column))
                    .collect::<Vec<_>>()
                    .join(", "),
                placeholders.join(", ")
            );
            let mut statement = conn
                .prepare(&sql)
                .map_err(|error| format!("prepare insert {table}: {error}"))?;
            let params: Vec<rusqlite::types::Value> = columns
                .iter()
                .map(|column| object.get(column).map(sql_value).unwrap_or(rusqlite::types::Value::Null))
                .collect();
            statement
                .execute(rusqlite::params_from_iter(params.iter()))
                .map_err(|error| format!("insert into {table}: {error}"))?;
        }
    }
    // Optional integrity report: violations are returned, never fatal.
    let mut violations = Vec::new();
    if let Ok(mut statement) = conn.prepare("PRAGMA foreign_key_check") {
        if let Ok(mut rows) = statement.query([]) {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(table) = row.get::<_, String>(0) {
                    violations.push(format!("foreign_key_check: {table}"));
                }
            }
        }
    }
    drop(conn);
    Ok((out, violations))
}

fn replay_sqlite(
    archive: &Archive,
    input: &SourceInput,
    domain: &str,
    prepared: &Prepared,
) -> Result<(Vec<DerivedSession>, Vec<String>), ArchiveError> {
    let replay = crate::capture_sqlite::replay_sqlite(archive, &input.source, None, domain)?;
    let baselines = crate::capture_sqlite::sqlite_baselines(archive, &input.source)?;
    let baseline = baselines
        .iter()
        .find(|baseline| baseline.capture_seq == replay.baseline_capture_seq)
        .ok_or_else(|| {
            ArchiveError::Integrity(format!(
                "SQLite replay for {} references missing baseline {}",
                input.source, replay.baseline_capture_seq
            ))
        })?;
    let key = source_key(&input.source);
    let (db, fk_violations) =
        materialize_observation_in(baseline.profile(), &replay, &baseline.ddl, prepared, &key)
            .map_err(|error| ArchiveError::Corrupt(format!("{}: {error}", input.source)))?;
    let mut session_ids: Vec<String> = Vec::new();
    let mut found_table = false;
    for table in ["session_v2", "session"] {
        if let Some(rows) = replay.tables.get(table) {
            found_table = true;
            session_ids.extend(
                rows.values()
                    .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string)),
            );
        }
    }
    if !found_table {
        return Err(ArchiveError::Corrupt(format!(
            "{}: replayed observation has no session table",
            input.source
        )));
    }
    session_ids.sort();
    session_ids.dedup();
    let mut sessions = Vec::new();
    let mut anomalies = Vec::new();
    // foreign_key_check findings are reported (the rebuild itself is
    // foreign-key-safe), never silently swallowed.
    anomalies.extend(fk_violations);
    for session_id in session_ids {
        if !session_id.starts_with("ses_") {
            // The ported loader's SQLite reference contract requires an
            // OpenCode-style `ses_` id. Anything else is reported, never guessed.
            anomalies.push(format!(
                "session {session_id:?} does not use the ses_ id shape; not parseable by the ported loader"
            ));
            continue;
        }
        let source_ref = format!("sqlite:{}:{}", db.display(), session_id);
        match crate::providers::opencode::load_messages_sqlite(&source_ref) {
            Ok(messages) => sessions.push(DerivedSession {
                session_id,
                messages,
            }),
            Err(error) => {
                anomalies.push(format!("session {session_id}: {error}"));
            }
        }
    }
    Ok((sessions, anomalies))
}

// -- replay driver -----------------------------------------------------------

fn checkpoint_consistent(
    archive: &Archive,
    entry: &DerivedSourceEntry,
    input: &SourceInput,
    generation_status: &str,
) -> bool {
    if generation_status == DERIVED_GENERATION_REBUILDING || entry.status != "ok" {
        return false;
    }
    if entry.provider != input.provider || entry.kind != input.kind {
        return false;
    }
    if entry.input_kind != input.input_kind {
        return false;
    }
    if entry.input_kind == "sqlite_observation" {
        if entry.capture_seq != input.capture_seq {
            return false;
        }
    } else {
        let boundary = input
            .records
            .last()
            .map(|record| record.sha256.clone())
            .unwrap_or_default();
        if entry.capture_generation != input.generation
            || entry.records != input.records.len() as u64
            || entry.record_boundary_sha != boundary
            || entry.input_digest != input.input_digest
        {
            return false;
        }
    }
    let Some(relative) = entry.artifact.as_deref() else {
        return false;
    };
    artifact_digest(archive, relative)
        .map(|digest| digest == entry.artifact_sha256)
        .unwrap_or(false)
}

fn entry_messages(archive: &Archive, entry: &DerivedSourceEntry) -> Option<(u64, u64)> {
    let relative = entry.artifact.as_deref()?;
    let bytes = fs::read(artifact_abs(archive, relative)).ok()?;
    let artifact: DerivedArtifact = serde_json::from_slice(&bytes).ok()?;
    let messages: u64 = artifact
        .sessions
        .iter()
        .map(|session| session.messages.len() as u64)
        .sum();
    Some((messages, artifact.sessions.len() as u64))
}


/// Replay the archive into the derived layer.
///
/// `domain` is the access domain used for every object read (fail-closed). The
/// retrieval index is built with the lexical generator; an embedding generation
/// is produced by [`replay_with_plan`].
pub fn replay(
    archive: &Archive,
    identity: &ReplayIdentity,
    domain: &str,
) -> Result<ReplayReport, ArchiveError> {
    replay_with_generator(archive, identity, domain, &RetrievalGenerator::lexical())
}

/// Replay with an explicit generator. `embedding` is required when the generator
/// is an embedding generation (`ArchiveError::Embedding` otherwise); a lexical
/// generator never writes vectors.
pub fn replay_with_generator(
    archive: &Archive,
    identity: &ReplayIdentity,
    domain: &str,
    generator: &RetrievalGenerator,
) -> Result<ReplayReport, ArchiveError> {
    replay_inner(archive, identity, domain, generator, None)
}

/// Replay with the configured embedding plan: units are projected from the
/// committed records, embedded once, and written to the same derived generation
/// as the index. A model failure is an error here so the caller can decide to
/// rebuild lexically and report the degradation; it never corrupts the archive.
pub fn replay_with_plan(
    archive: &Archive,
    identity: &ReplayIdentity,
    domain: &str,
    plan: &crate::recall::semantic::embed::EmbeddingPlan,
) -> Result<ReplayReport, ArchiveError> {
    let generator = RetrievalGenerator::embedding(&plan.spec);
    replay_inner(archive, identity, domain, &generator, Some(plan))
}

fn replay_inner(
    archive: &Archive,
    identity: &ReplayIdentity,
    domain: &str,
    generator: &RetrievalGenerator,
    embedding: Option<&crate::recall::semantic::embed::EmbeddingPlan>,
) -> Result<ReplayReport, ArchiveError> {
    if domain.is_empty() {
        return Err(ArchiveError::AccessDenied(
            "empty access domain (fail-closed)".to_string(),
        ));
    }
    let _lock = archive.acquire_derived_lock()?;

    let manifests_before = archive.manifests()?;
    let manifest_digest_before = manifest_digest(&manifests_before);
    let inputs = collect_inputs(archive)?;
    let prepared = Prepared::new(archive)?;

    let mut state = load_state(archive)?;
    // A generation is reused only for the same parser identity **and** the same
    // retrieval generator key: switching the embedding model must allocate a new
    // generation instead of overwriting another model's vectors in place.
    let wanted_key = generator.generation_key.clone();
    let wanted_quantization = crate::recall::semantic::index::VECTORS_QUANTIZATION;
    let existing_index = state.generations.iter().position(|generation| {
        &generation.identity == identity
            && match generation.retrieval.as_ref() {
                Some(retrieval) => {
                    retrieval.generation_key == wanted_key
                        && (generator.kind != "embedding"
                            || retrieval
                                .vectors
                                .as_ref()
                                .map(|vectors| vectors.quantization == wanted_quantization)
                                .unwrap_or(false))
                }
                None => generator.kind == "lexical",
            }
    });
    let (generation_number, mode) = match existing_index {
        Some(index) => (state.generations[index].generation, "resumed"),
        None => (
            state
                .generations
                .iter()
                .map(|generation| generation.generation)
                .max()
                .unwrap_or(0)
                + 1,
            "new_generation",
        ),
    };
    let now = now_ms();
    let mut generation = match existing_index {
        Some(index) => state.generations.remove(index),
        None => DerivedGeneration {
            generation: generation_number,
            identity: identity.clone(),
            created_at_ms: now,
            updated_at_ms: now,
            status: DERIVED_GENERATION_COMPLETE.to_string(),
            sources: Vec::new(),
            anomalies: Vec::new(),
            retrieval: None,
        },
    };
    // Fail-closed marker: if this process dies mid-rebuild, readers see
    // `rebuilding` and deny, and the next replay refuses to reuse entries.
    let prior_status = generation.status.clone();
    generation.status = DERIVED_GENERATION_REBUILDING.to_string();
    generation.updated_at_ms = now;
    state.generations.push(generation.clone());
    save_state(archive, &state)?;
    state.generations.pop();

    let previous_entries: BTreeMap<String, DerivedSourceEntry> = generation
        .sources
        .iter()
        .map(|entry| (entry.source.clone(), entry.clone()))
        .collect();
    let mut new_entries: Vec<DerivedSourceEntry> = Vec::new();
    let mut reports: Vec<ReplaySourceReport> = Vec::new();
    let mut anomalies: Vec<String> = Vec::new();
    if prior_status == DERIVED_GENERATION_REBUILDING {
        anomalies.push(format!(
            "generation {generation_number} was left in rebuilding state by an interrupted run; entries were replayed from the archive instead of reused"
        ));
    }

    for input in &inputs {
        let key = source_key(&input.source);
        let previous = previous_entries.get(&input.source);
        if let Some(previous) = previous {
            if checkpoint_consistent(archive, previous, input, &prior_status) {
                let (messages, sessions) = entry_messages(archive, previous).unwrap_or((0, 0));
                new_entries.push(previous.clone());
                reports.push(ReplaySourceReport {
                    source: input.source.clone(),
                    provider: input.provider.clone(),
                    kind: input.kind.clone(),
                    status: "reused".to_string(),
                    input_kind: input.input_kind.clone(),
                    records: input.records.len() as u64,
                    records_replayed: 0,
                    capture_generation: input.generation,
                    capture_seq: input.capture_seq,
                    messages,
                    sessions,
                    artifact: previous.artifact.clone(),
                    artifact_sha256: Some(previous.artifact_sha256.clone()),
                    derived_ref: Some(format!(
                        "derived:{generation_number}:{key}"
                    )),
                    checkpoint_reused: true,
                    error: None,
                    anomalies: previous.anomalies.clone(),
                });
                continue;
            }
            if previous.status == "ok" {
                anomalies.push(format!(
                    "{}: checkpoint did not match the archive; replayed from raw objects",
                    input.source
                ));
            }
        }

        if input.input_kind == "records"
            && input.records.is_empty()
            && input.stream_objects.is_empty()
        {
            let entry = DerivedSourceEntry {
                source: input.source.clone(),
                provider: input.provider.clone(),
                kind: input.kind.clone(),
                key: key.clone(),
                capture_generation: input.generation,
                capture_seq: 0,
                input_kind: input.input_kind.clone(),
                records: 0,
                record_boundary_sha: String::new(),
                input_digest: input.input_digest.clone(),
                access: input.access.clone(),
                messages: 0,
                sessions: 0,
                bytes: 0,
                artifact: None,
                artifact_sha256: String::new(),
                status: "empty".to_string(),
                error: None,
                anomalies: Vec::new(),
                replayed_at_ms: now_ms(),
            };
            new_entries.push(entry);
            reports.push(ReplaySourceReport {
                source: input.source.clone(),
                provider: input.provider.clone(),
                kind: input.kind.clone(),
                status: "empty".to_string(),
                input_kind: input.input_kind.clone(),
                records: 0,
                records_replayed: 0,
                capture_generation: input.generation,
                capture_seq: input.capture_seq,
                messages: 0,
                sessions: 0,
                artifact: None,
                artifact_sha256: None,
                derived_ref: None,
                checkpoint_reused: false,
                error: None,
                anomalies: Vec::new(),
            });
            continue;
        }

        if !crate::SUPPORTED_PROVIDERS.contains(&input.provider.as_str())
            || !matches!(
                input.kind.as_str(),
                "jsonl_file" | "whole_file" | "compressed_jsonl_file" | "sqlite"
            )
        {
            let error = format!(
                "unsupported provider/kind {:?}/{:?}; not in the P0 support matrix",
                input.provider, input.kind
            );
            new_entries.push(DerivedSourceEntry {
                source: input.source.clone(),
                provider: input.provider.clone(),
                kind: input.kind.clone(),
                key: key.clone(),
                capture_generation: input.generation,
                capture_seq: input.capture_seq,
                input_kind: input.input_kind.clone(),
                records: input.records.len() as u64,
                record_boundary_sha: input
                    .records
                    .last()
                    .map(|record| record.sha256.clone())
                    .unwrap_or_default(),
                input_digest: input.input_digest.clone(),
                access: input.access.clone(),
                messages: 0,
                sessions: 0,
                bytes: 0,
                artifact: None,
                artifact_sha256: String::new(),
                status: "unsupported".to_string(),
                error: Some(error.clone()),
                anomalies: Vec::new(),
                replayed_at_ms: now_ms(),
            });
            reports.push(ReplaySourceReport {
                source: input.source.clone(),
                provider: input.provider.clone(),
                kind: input.kind.clone(),
                status: "unsupported".to_string(),
                input_kind: input.input_kind.clone(),
                records: input.records.len() as u64,
                records_replayed: 0,
                capture_generation: input.generation,
                capture_seq: input.capture_seq,
                messages: 0,
                sessions: 0,
                artifact: None,
                artifact_sha256: None,
                derived_ref: None,
                checkpoint_reused: false,
                error: Some(error.clone()),
                anomalies: Vec::new(),
            });
            anomalies.push(format!("{}: {error}", input.source));
            continue;
        }

        let parsed: Result<(Vec<DerivedSession>, Vec<String>), String> = match input.input_kind.as_str() {
            "sqlite_observation" => replay_sqlite(archive, input, domain, &prepared)
                .map_err(|error| error.to_string())
                .map(|(sessions, session_anomalies)| (sessions, session_anomalies)),
            _ => replay_records(archive, input, domain, &prepared)
                .map_err(|error| error.to_string())
                .map(|sessions| (sessions, Vec::new())),
        };

        match parsed {
            Ok((sessions, source_anomalies)) => {
                let (relative, sha, bytes) =
                    write_artifact(archive, generation_number, input, identity, sessions.clone())?;
                let messages: u64 = sessions
                    .iter()
                    .map(|session| session.messages.len() as u64)
                    .sum();
                let entry = DerivedSourceEntry {
                    source: input.source.clone(),
                    provider: input.provider.clone(),
                    kind: input.kind.clone(),
                    key: key.clone(),
                    capture_generation: input.generation,
                    capture_seq: input.capture_seq,
                    input_kind: input.input_kind.clone(),
                    records: input.records.len() as u64,
                    record_boundary_sha: input
                        .records
                        .last()
                        .map(|record| record.sha256.clone())
                        .unwrap_or_default(),
                    input_digest: input.input_digest.clone(),
                    access: input.access.clone(),
                    messages,
                    sessions: sessions.len() as u64,
                    bytes,
                    artifact: Some(relative.clone()),
                    artifact_sha256: sha.clone(),
                    status: "ok".to_string(),
                    error: None,
                    anomalies: source_anomalies.clone(),
                    replayed_at_ms: now_ms(),
                };
                new_entries.push(entry);
                reports.push(ReplaySourceReport {
                    source: input.source.clone(),
                    provider: input.provider.clone(),
                    kind: input.kind.clone(),
                    status: "replayed".to_string(),
                    input_kind: input.input_kind.clone(),
                    records: input.records.len() as u64,
                    records_replayed: input.records.len() as u64,
                    capture_generation: input.generation,
                    capture_seq: input.capture_seq,
                    messages,
                    sessions: sessions.len() as u64,
                    artifact: Some(relative),
                    artifact_sha256: Some(sha),
                    derived_ref: Some(format!("derived:{generation_number}:{key}")),
                    checkpoint_reused: false,
                    error: None,
                    anomalies: source_anomalies,
                });
            }
            Err(error) => {
                let entry = DerivedSourceEntry {
                    source: input.source.clone(),
                    provider: input.provider.clone(),
                    kind: input.kind.clone(),
                    key: key.clone(),
                    capture_generation: input.generation,
                    capture_seq: input.capture_seq,
                    input_kind: input.input_kind.clone(),
                    records: input.records.len() as u64,
                    record_boundary_sha: input
                        .records
                        .last()
                        .map(|record| record.sha256.clone())
                        .unwrap_or_default(),
                    input_digest: input.input_digest.clone(),
                    access: input.access.clone(),
                    messages: 0,
                    sessions: 0,
                    bytes: 0,
                    artifact: None,
                    artifact_sha256: String::new(),
                    status: "parse_failed".to_string(),
                    error: Some(error.clone()),
                    anomalies: Vec::new(),
                    replayed_at_ms: now_ms(),
                };
                new_entries.push(entry);
                reports.push(ReplaySourceReport {
                    source: input.source.clone(),
                    provider: input.provider.clone(),
                    kind: input.kind.clone(),
                    status: "parse_failed".to_string(),
                    input_kind: input.input_kind.clone(),
                    records: input.records.len() as u64,
                    records_replayed: input.records.len() as u64,
                    capture_generation: input.generation,
                    capture_seq: input.capture_seq,
                    messages: 0,
                    sessions: 0,
                    artifact: None,
                    artifact_sha256: None,
                    derived_ref: None,
                    checkpoint_reused: false,
                    error: Some(error.clone()),
                    anomalies: Vec::new(),
                });
                anomalies.push(format!("{}: replay parse failed: {error}", input.source));
            }
        }
    }

    // Keep entries for sources that no longer exist in the archive (history is
    // not silently rewritten), but never count them as current coverage.
    for (source, entry) in &previous_entries {
        if !new_entries.iter().any(|candidate| &candidate.source == source) {
            new_entries.push(entry.clone());
        }
    }

    generation.sources = new_entries;
    generation.status = if reports
        .iter()
        .any(|report| report.status == "parse_failed" || report.status == "unsupported")
    {
        DERIVED_GENERATION_PARTIAL.to_string()
    } else {
        DERIVED_GENERATION_COMPLETE.to_string()
    };
    generation.anomalies = anomalies.clone();
    // P1: rebuild the retrieval skeleton for this generation. It is derived and
    // purely rebuildable; failure here never touches evidence.
    let (retrieval, semantic) = match build_retrieval_index(
        archive,
        generation_number,
        domain,
        &generation.sources,
        identity,
        generator,
        embedding,
    ) {
        Ok(value) => value,
        Err(error) => {
            // An explicit failure (a missing model, a missing helper) is not a
            // crash: remove the fail-closed `rebuilding` marker so the previous
            // complete generation stays readable. A hard kill still leaves the
            // marker in place on purpose.
            let _ = save_state(archive, &state);
            return Err(error);
        }
    };
    generation.retrieval = Some(retrieval.clone());
    generation.updated_at_ms = now_ms();
    state.generations.push(generation);
    save_state(archive, &state)?;

    let manifests_after = archive.manifests()?;
    let manifest_digest_after = manifest_digest(&manifests_after);
    let replayed_sources = reports
        .iter()
        .filter(|report| report.status == "replayed")
        .count() as u64;
    let reused_sources = reports
        .iter()
        .filter(|report| report.status == "reused")
        .count() as u64;
    let failed_sources = reports
        .iter()
        .filter(|report| {
            report.status == "parse_failed" || report.status == "unsupported"
        })
        .count() as u64;
    let messages = reports.iter().map(|report| report.messages).sum();
    Ok(ReplayReport {
        ok: failed_sources == 0 && manifest_digest_before == manifest_digest_after,
        mode: mode.to_string(),
        generation: generation_number,
        identity: identity.clone(),
        sources: reports,
        reused_sources,
        replayed_sources,
        failed_sources,
        messages,
        manifest_digest_before,
        manifest_digest_after,
        derived_state_path: state_path(archive).to_string_lossy().to_string(),
        retrieval: Some(retrieval),
        semantic,
        anomalies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::capture_source;

    #[test]
    fn source_keys_are_stable_and_bounded() {
        let key = source_key("C:/synthetic/rollout-1.jsonl");
        assert_eq!(key.len(), 32);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(key, source_key("C:/synthetic/rollout-1.jsonl"));
        assert_ne!(key, source_key("C:/synthetic/rollout-2.jsonl"));
    }

    #[test]
    fn hex_marker_decoding_is_strict() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("zz"), None);
    }

    /// P5 defect regression (stage-2 S1): a V2 database with real foreign keys
    /// whose delta lines list child rows before parent rows must still
    /// materialize. The rebuild runs with `PRAGMA foreign_keys = OFF` and
    /// parent-first insert order, and `foreign_key_check` stays clean.
    #[test]
    fn sqlite_materialization_is_foreign_key_safe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive = crate::archive::Archive::init(temp.path()).expect("archive");
        let db = temp.path().join("opencode.db");
        {
            let conn = rusqlite::Connection::open(&db).expect("open fixture");
            conn.execute_batch(
                r#"
                CREATE TABLE session_v2 (
                    id TEXT PRIMARY KEY, title TEXT, directory TEXT,
                    time_created INTEGER, time_updated INTEGER
                );
                CREATE TABLE session_message (
                    id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT,
                    time_created INTEGER, time_updated INTEGER, data TEXT,
                    FOREIGN KEY (session_id) REFERENCES session_v2(id) ON DELETE CASCADE
                );
                INSERT INTO session_v2 VALUES ('ses_1', 'one', 'C:/w', 1, 2);
                INSERT INTO session_message VALUES
                    ('sm_1', 'ses_1', 1, 'user', 10, 10, '{"text":"hello"}');
                "#,
            )
            .expect("fixture schema");
        }
        let report = capture_source(&archive, &db, "opencode", "default", None).expect("capture");
        assert!(report.ok, "{report:?}");
        let evidence_source = report.source.clone();
        // One observation window touches parent and child; the committed delta
        // line order is (table, key), which puts the child row
        // (session_message) before its parent (session_v2) — exactly the
        // out-of-order stream that broke the P5 materialization.
        {
            let conn = rusqlite::Connection::open(&db).expect("reopen");
            conn.execute_batch(
                "INSERT INTO session_v2 VALUES
                     ('ses_2', 'two', 'C:/w', 2, 20);
                 INSERT INTO session_message VALUES
                     ('sm_2', 'ses_2', 1, 'user', 20, 20, '{\"text\":\"after-parent\"}');",
            )
            .expect("parent and child insert");
        }
        let delta = capture_source(&archive, &db, "opencode", "default", None).expect("delta");
        assert_eq!(delta.change, "delta", "{delta:?}");
        let replay =
            crate::capture_sqlite::replay_sqlite(&archive, &evidence_source, None, "default")
                .expect("replay");
        let batches = crate::capture_sqlite::sqlite_deltas(&archive, &evidence_source, "default")
            .expect("deltas");
        let lines = &batches[0].lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].table, "session_message", "child line first");
        assert_eq!(lines[1].table, "session_v2");

        let baselines = crate::capture_sqlite::sqlite_baselines(&archive, &evidence_source)
            .expect("baselines");
        assert!(!baselines[0].ddl.is_empty());
        let prepared = Prepared::new(&archive).expect("prepared");
        let (materialized, violations) = materialize_observation(
            &replay,
            &baselines[0].ddl,
            &prepared,
            &source_key(&replay.source),
        )
        .expect("materialize");
        assert!(
            violations.is_empty(),
            "foreign_key_check must be clean: {violations:?}"
        );
        let conn = rusqlite::Connection::open(&materialized).expect("open materialized");
        let sessions: i64 = conn
            .query_row("SELECT count(*) FROM session_v2", [], |row| row.get(0))
            .expect("session count");
        let messages: i64 = conn
            .query_row("SELECT count(*) FROM session_message", [], |row| row.get(0))
            .expect("message count");
        assert_eq!(sessions, 2);
        assert_eq!(messages, 2);
    }
}
