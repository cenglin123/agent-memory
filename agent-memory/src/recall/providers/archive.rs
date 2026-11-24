//! Archive provider (P1): read-only lexical recall over committed capture
//! records.
//!
//! # What is searched
//!
//! Only capture facts are read: committed manifests (`archive/manifests/`) and
//! immutable content-addressed objects (`archive/objects/<aa>/<bb>/<sha256>`).
//! This provider never captures, never writes a manifest, never calls
//! `capture_source` / `write_rollout` and never acquires the capture lock.
//!
//! Evidence selection mirrors the derived replay: a whole-file or compressed
//! source is represented by its **last** manifest (a full capture is a
//! replacement), every other source by the manifests of its **current capture
//! generation**. Superseded generations stay on disk but are not searched, so a
//! forced recapture cannot surface both copies as separate hits.
//!
//! # Identity
//!
//! `rel_path` is the real object address
//! (`archive/objects/<aa>/<bb>/<object sha256>`) and the locator carries only
//! `record_sha256` + the projection offset. `capture_id` and `generation` are
//! never part of `hit_id`, so the same content captured twice yields the same
//! `hit_id` and the same `range_sha256`. The projection is the **decoded**
//! record (`.jsonl.zst` objects store the decompressed bytes), so a plain and a
//! compressed carrier of the same content share one identity and one readback
//! baseline.
//!
//! # Access
//!
//! Only Archive records participate in the access-label predicate: a record is
//! readable iff its label set is non-empty and contains the effective domain.
//! Empty labels fail closed. A source whose every record is rejected reports
//! `unavailable` with `DOMAIN_DENIED` / `DOMAIN_UNREACHABLE`, never a healthy
//! empty result. Live sources (KB / device / OpenCode / Codex) are unaffected by
//! this predicate.

use crate::archive::{
    hex_sha256, Archive, ArchiveError, Manifest, ManifestRecord, ATTACHMENT_SOURCE_KIND,
};
use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{
    codes, status, RANK_BASIS_LEXICAL_WINDOW, RANK_BASIS_SEMANTIC_BONUS,
    SCHEMA_ARCHIVE_RECORD_V1, SCORE_KIND_NONE, SCORE_KIND_SEMANTIC_BONUS, SOURCE_KIND_ARCHIVE,
};
use crate::recall::domain::{classify_access, AccessDecision};
use crate::recall::graph::{self, RelatedItem, SourceGraph};
use crate::recall::hash::{root_sha256, sha256_hex, TextRange};
use crate::recall::providers::{rank_hits, SourceHit, SourceOutcome};
use crate::recall::refs::{hit_id, Fingerprint, HitRef, Locator};
use crate::recall::semantic::embed::EmbeddingPlan;
use crate::recall::semantic::index::{self as semantic_index, SemanticIndex};
use crate::recall::semantic::units::{extract as extract_units, normalize_unit_text, unit_sha256};
use crate::recall::text::{fold_ascii, lexical_match, snippet};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

pub const OBJECTS_PREFIX: &str = "archive/objects";

/// Derived session-mapping states. `unmapped` is a first-class answer: an
/// unknown mapping is never guessed from the file name or the newest session.
pub const SESSION_MAPPING_OK: &str = "ok";
pub const SESSION_MAPPING_ABSENT: &str = "absent";
pub const SESSION_MAPPING_IDENTITY_MISMATCH: &str = "identity_mismatch";
pub const SESSION_MAPPING_UNREADABLE: &str = "unreadable";
/// The derived index is readable but its frozen access labels do not contain the
/// effective domain. A mapping (like a vector) from another domain is never used.
pub const SESSION_MAPPING_ACCESS_MISMATCH: &str = "access_mismatch";

/// `request > configuration > default`, resolved by the caller.
#[derive(Debug, Clone, Copy)]
pub struct DomainSelection<'a> {
    pub effective: &'a str,
    pub explicit: bool,
}

pub struct ArchiveRequest<'a> {
    pub source: &'a SourceConfig,
    pub terms: &'a [String],
    /// Original query text, used only to embed the query when a semantic
    /// generation is configured and valid.
    pub query: &'a str,
    pub budget: &'a Budget,
    pub snippet_bytes: usize,
    pub session_id: Option<&'a str>,
    pub domain: DomainSelection<'a>,
    pub read_root: &'a Path,
    /// `Some` when the configuration carries an embedding spec. `None` keeps the
    /// P1 lexical-only path byte-identical.
    pub embedding: Option<&'a EmbeddingPlan>,
    /// P4: `Some(1..=2)` asks for bounded `next`-edge expansion of every hit.
    /// `None` keeps the P3 response byte-identical (no `related` key at all).
    pub related_hops: Option<usize>,
}

use crate::recall::semantic::relevance::{
    ARCHIVE_RELEVANCE_VERSION, LEXICAL_ACCEPT_COVERAGE, LEXICAL_ACCEPT_DISTINCT, LEXICAL_BONUS,
    SCORE_FUSION, SEMANTIC_ACCEPT_SIMILARITY, SEMANTIC_MAX_CANDIDATES,
    SEMANTIC_SUPPORT_SIMILARITY, SEMANTIC_SUPPORT_UNITS, SEMANTIC_TURN_MAX_SIMILARITY,
    TURN_AGGREGATION_BONUS,
};

/// Lexical coverage of a hit (0–1). Single-term queries are either matched or not.
fn lexical_coverage(distinct_terms: usize, query_terms: usize) -> f64 {
    if query_terms == 0 {
        return 0.0;
    }
    (distinct_terms as f64 / query_terms as f64).clamp(0.0, 1.0)
}

fn lexical_accepts(distinct_terms: usize, query_terms: usize) -> bool {
    if query_terms <= 1 {
        return distinct_terms >= 1;
    }
    distinct_terms >= LEXICAL_ACCEPT_DISTINCT
        && lexical_coverage(distinct_terms, query_terms) >= LEXICAL_ACCEPT_COVERAGE
}

/// A rejected Archive provider open: the frozen `(status, code)` triple plus a
/// machine-readable reason.
#[derive(Debug, Clone)]
pub(crate) struct ArchiveFailure {
    pub status: &'static str,
    pub code: &'static str,
    pub reason: &'static str,
    pub message: String,
}

/// One committed capture record selected as evidence.
#[derive(Debug, Clone)]
pub(crate) struct Unit {
    /// Physical source path recorded by the collector (a capture fact).
    pub source: String,
    pub provider: String,
    pub captured_at_ms: i64,
    pub record: ManifestRecord,
    /// Ordering key inside one source: source byte offset, then record seq.
    pub order: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Bytes,
    Time,
}

// ---------------------------------------------------------------------------
// Open / read helpers shared with `memory_read`
// ---------------------------------------------------------------------------

pub(crate) fn open(read_root: &Path) -> Result<Archive, ArchiveFailure> {
    let archive = Archive::open(read_root).map_err(|error| match error {
        ArchiveError::NotInitialized(_) => ArchiveFailure {
            status: status::UNAVAILABLE,
            code: codes::SOURCE_ROOT_INVALID,
            reason: "archive_root_not_initialized",
            message: "archive root is not initialized (state/schema.json is missing)".to_string(),
        },
        other => ArchiveFailure {
            status: status::UNAVAILABLE,
            code: codes::DB_UNAVAILABLE,
            reason: "archive_root_unreadable",
            message: format!("archive root cannot be opened: {other}"),
        },
    })?;
    let schema = archive.schema().map_err(|error| ArchiveFailure {
        status: status::UNAVAILABLE,
        code: codes::SCHEMA_UNSUPPORTED,
        reason: "archive_schema_unreadable",
        message: format!("archive state/schema.json cannot be read: {error}"),
    })?;
    // Generation mismatch is explicit: an archive written by another schema or
    // derived model generation is not silently read with the current rules.
    if schema.schema_version != crate::archive::ARCHIVE_SCHEMA_VERSION {
        return Err(ArchiveFailure {
            status: status::UNAVAILABLE,
            code: codes::SCHEMA_UNSUPPORTED,
            reason: "archive_schema_generation_mismatch",
            message: format!(
                "archive schema_version {} is not supported (expected {})",
                schema.schema_version,
                crate::archive::ARCHIVE_SCHEMA_VERSION
            ),
        });
    }
    if schema.derived.model_version != crate::archive::DERIVED_MODEL_VERSION {
        return Err(ArchiveFailure {
            status: status::UNAVAILABLE,
            code: codes::SCHEMA_UNSUPPORTED,
            reason: "archive_derived_generation_mismatch",
            message: format!(
                "archive derived model_version {} is not supported (expected {})",
                schema.derived.model_version,
                crate::archive::DERIVED_MODEL_VERSION
            ),
        });
    }
    Ok(archive)
}

pub(crate) fn units(archive: &Archive) -> Result<Vec<Unit>, ArchiveError> {
    let manifests = archive.manifests()?;
    Ok(load_units(&manifests))
}

/// Select the evidence units from one committed-manifest snapshot.
pub(crate) fn load_units(manifests: &[Manifest]) -> Vec<Unit> {
    let mut groups: BTreeMap<String, Vec<&Manifest>> = BTreeMap::new();
    for manifest in manifests {
        // Attachment manifests are attachment evidence units, not captures of a
        // session source; they are not part of the recall corpus.
        if manifest.source.kind == ATTACHMENT_SOURCE_KIND {
            continue;
        }
        groups
            .entry(manifest.source.path.clone())
            .or_default()
            .push(manifest);
    }

    let mut out: Vec<Unit> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (source, group) in groups {
        let last = *group.last().expect("non-empty group");
        let kind = last.source.kind.clone();
        let selected: Vec<&Manifest> = if kind == "whole_file" || kind == "compressed_jsonl_file" {
            vec![last]
        } else {
            let generation = group
                .iter()
                .map(|manifest| manifest.source.generation)
                .max()
                .unwrap_or(1);
            group
                .iter()
                .filter(|manifest| manifest.source.generation == generation)
                .copied()
                .collect()
        };
        let mut source_units: Vec<Unit> = Vec::new();
        for manifest in selected {
            for record in &manifest.records {
                // One record content is one corpus unit per physical source,
                // even when a later manifest re-references it.
                if !seen.insert((source.clone(), record.sha256.clone())) {
                    continue;
                }
                source_units.push(Unit {
                    source: source.clone(),
                    provider: manifest.provider.clone(),
                    captured_at_ms: manifest.captured_at_ms,
                    record: record.clone(),
                    order: record.source_offset.unwrap_or(record.offset),
                });
            }
        }
        source_units.sort_by(|a, b| {
            a.order
                .cmp(&b.order)
                .then_with(|| a.record.seq.cmp(&b.record.seq))
        });
        out.extend(source_units);
    }
    out
}

/// Strictest per-object access intersection over **all** committed manifests.
///
/// This is the same predicate as `Archive::object_accessible` (a domain must
/// appear in every reference; an empty label set or an unreferenced object is
/// unreadable), computed once from the manifest snapshot a caller already holds
/// instead of re-listing the manifests for every object read.
pub(crate) fn object_access(manifests: &[Manifest]) -> BTreeMap<String, BTreeSet<String>> {
    let mut map: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
    for manifest in manifests {
        for object in &manifest.objects {
            let entry = map.entry(object.sha256.clone()).or_insert_with(|| {
                if object.access.is_empty() {
                    None
                } else {
                    Some(object.access.iter().cloned().collect())
                }
            });
            // `or_insert_with` already consumed this reference for a new entry;
            // every later reference narrows the intersection.
            if let Some(existing) = entry.as_mut() {
                if object.access.is_empty() {
                    *entry = None;
                    continue;
                }
                existing.retain(|label| object.access.iter().any(|value| value == label));
                if existing.is_empty() {
                    *entry = None;
                }
            }
        }
    }
    map.into_iter()
        .filter_map(|(sha, labels)| labels.map(|labels| (sha, labels)))
        .collect()
}

/// `archive/objects/<aa>/<bb>/<sha256>` for a real content address.
pub(crate) fn object_rel_path(object_sha256: &str) -> Option<String> {
    if !is_sha256(object_sha256) {
        return None;
    }
    Some(format!(
        "{OBJECTS_PREFIX}/{}/{}/{}",
        &object_sha256[0..2],
        &object_sha256[2..4],
        object_sha256
    ))
}

/// Inverse of [`object_rel_path`]: only the exact real object address parses.
pub(crate) fn object_sha_from_rel_path(rel_path: &str) -> Option<String> {
    let rest = rel_path.strip_prefix(OBJECTS_PREFIX)?.strip_prefix('/')?;
    let mut parts = rest.split('/');
    let aa = parts.next()?;
    let bb = parts.next()?;
    let sha = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if !is_sha256(sha) || aa != &sha[0..2] || bb != &sha[2..4] {
        return None;
    }
    Some(sha.to_string())
}

/// Searchable projection of one committed record: the **decoded** record bytes
/// as UTF-8 (a `.jsonl.zst` object already holds decompressed bytes) with
/// trailing CR/LF removed. `None` means the record is not projectedable (not
/// UTF-8, or blank) and is counted, never turned into an unreadable hit.
pub(crate) fn project_record(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub(crate) fn read_object(
    archive: &Archive,
    domain: &str,
    object_sha256: &str,
) -> Result<Vec<u8>, ArchiveError> {
    archive.reader(domain).read_object(object_sha256)
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// ---------------------------------------------------------------------------
// Derived session mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct SessionMap {
    pub status: &'static str,
    pub generation: Option<u64>,
    /// Source path -> derived session id. `None` is `unmapped` and is never
    /// replaced by a guess.
    pub by_source: BTreeMap<String, Option<String>>,
}

impl SessionMap {
    fn absent() -> Self {
        SessionMap {
            status: SESSION_MAPPING_ABSENT,
            generation: None,
            by_source: BTreeMap::new(),
        }
    }

    pub(crate) fn session_for(&self, source: &str) -> Option<&str> {
        if self.status != SESSION_MAPPING_OK {
            return None;
        }
        self.by_source.get(source)?.as_deref()
    }
}

/// Read the derived retrieval index of the current replay generation.
///
/// Session ownership is a derived fact: when the index was built by another
/// parser generation, another unit parser, another replay identity or with access
/// labels that do not contain the effective domain, the mapping is an explicit
/// mismatch (`identity_mismatch` / `access_mismatch`), not silently reused.
/// Unknown mappings stay `unmapped`.
pub(crate) fn session_map(read_root: &Path, domain: &str) -> SessionMap {
    let state_path = read_root.join("derived").join("state.json");
    let raw = match std::fs::read(&state_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SessionMap::absent()
        }
        Err(_) => {
            return SessionMap {
                status: SESSION_MAPPING_UNREADABLE,
                generation: None,
                by_source: BTreeMap::new(),
            }
        }
    };
    let state: Value = match serde_json::from_slice(&raw) {
        Ok(value) => value,
        Err(_) => {
            return SessionMap {
                status: SESSION_MAPPING_UNREADABLE,
                generation: None,
                by_source: BTreeMap::new(),
            }
        }
    };
    let Some(generations) = state.get("generations").and_then(Value::as_array) else {
        return SessionMap::absent();
    };
    let Some(generation) = generations
        .iter()
        .max_by_key(|generation| generation.get("generation").and_then(Value::as_u64).unwrap_or(0))
    else {
        return SessionMap::absent();
    };
    let number = generation.get("generation").and_then(Value::as_u64);
    // An interrupted rebuild denies reads; its mapping is not a current fact.
    if generation.get("status").and_then(Value::as_str) == Some("rebuilding") {
        return SessionMap {
            status: SESSION_MAPPING_UNREADABLE,
            generation: number,
            by_source: BTreeMap::new(),
        };
    }
    let current = crate::replay::ReplayIdentity::current();
    let identity_ok = generation
        .get("identity")
        .and_then(|identity| serde_json::from_value::<crate::replay::ReplayIdentity>(identity.clone()).ok())
        .map(|identity| identity == current)
        .unwrap_or(false);
    if !identity_ok {
        return SessionMap {
            status: SESSION_MAPPING_IDENTITY_MISMATCH,
            generation: number,
            by_source: BTreeMap::new(),
        };
    }
    let Some(number) = number else {
        return SessionMap::absent();
    };
    let index_path = read_root
        .join("derived")
        .join("generations")
        .join(format!("g{number}"))
        .join("retrieval")
        .join("index.json");
    let raw = match std::fs::read(&index_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SessionMap {
                status: SESSION_MAPPING_ABSENT,
                generation: Some(number),
                by_source: BTreeMap::new(),
            }
        }
        Err(_) => {
            return SessionMap {
                status: SESSION_MAPPING_UNREADABLE,
                generation: Some(number),
                by_source: BTreeMap::new(),
            }
        }
    };
    let index: Value = match serde_json::from_slice(&raw) {
        Ok(value) => value,
        Err(_) => {
            return SessionMap {
                status: SESSION_MAPPING_UNREADABLE,
                generation: Some(number),
                by_source: BTreeMap::new(),
            }
        }
    };
    // N4: the mapping is only usable when the index itself was accepted for this
    // identity *and* access domain. `index.access` is the frozen strict
    // intersection of the indexed sources; empty labels fail closed.
    if index.get("schema").and_then(Value::as_str)
        != Some(crate::recall::semantic::index::RETRIEVAL_INDEX_SCHEMA)
        || index.get("unitParser").and_then(Value::as_str)
            != Some(crate::recall::semantic::index::RETRIEVAL_UNIT_PARSER)
    {
        return SessionMap {
            status: SESSION_MAPPING_IDENTITY_MISMATCH,
            generation: Some(number),
            by_source: BTreeMap::new(),
        };
    }
    let index_identity = index
        .get("identity")
        .and_then(|identity| serde_json::from_value::<crate::replay::ReplayIdentity>(identity.clone()).ok());
    if index_identity.as_ref() != Some(&current) {
        return SessionMap {
            status: SESSION_MAPPING_IDENTITY_MISMATCH,
            generation: Some(number),
            by_source: BTreeMap::new(),
        };
    }
    let access: Vec<String> = index
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
        return SessionMap {
            status: SESSION_MAPPING_ACCESS_MISMATCH,
            generation: Some(number),
            by_source: BTreeMap::new(),
        };
    }
    let mut by_source = BTreeMap::new();
    if let Some(sources) = index.get("sources").and_then(Value::as_array) {
        for source in sources {
            let Some(path) = source.get("source").and_then(Value::as_str) else {
                continue;
            };
            let session = source
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string());
            by_source.insert(path.to_string(), session);
        }
    }
    SessionMap {
        status: SESSION_MAPPING_OK,
        generation: Some(number),
        by_source,
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

enum ObjectLoad {
    Bytes(Vec<u8>),
    Missing,
    Corrupt,
}

struct Counters {
    records: u64,
    searched: u64,
    denied: u64,
    unreachable: u64,
    missing: u64,
    corrupt: u64,
    unprojectable: u64,
    unmapped: u64,
    /// Records whose access label mapping is missing (empty labels) or whose
    /// object has no readable committed reference. Fail-closed, and reported as
    /// an anomaly rather than as an ordinary out-of-scope record.
    labels_missing: u64,
}

impl Counters {
    fn new() -> Self {
        Counters {
            records: 0,
            searched: 0,
            denied: 0,
            unreachable: 0,
            missing: 0,
            corrupt: 0,
            unprojectable: 0,
            unmapped: 0,
            labels_missing: 0,
        }
    }
}

struct SemanticRuntime {
    index: SemanticIndex,
    query: Vec<f32>,
    query_norm: f32,
    /// `hit_id` -> best unit of that record, with its source and turn.
    candidates: BTreeMap<String, SemanticCandidate>,
}

struct SemanticCandidate {
    similarity: f64,
    hit: SourceHit,
    source: String,
    turn: u32,
}

/// Machine-readable semantic state reported in `detail.semantic`.
struct SemanticReport {
    status: &'static str,
    reason: Option<String>,
    generation_key: String,
    generation: Option<u64>,
    dimension: usize,
    units: usize,
    candidates: usize,
    fused: bool,
}

/// Machine-readable state of the P4 related expansion.
struct RelatedReport {
    hops: usize,
    nodes: usize,
    denied: usize,
    sidechain: usize,
    truncated: usize,
}

/// One candidate inside the semantic-dominant fusion step.
struct FusedCandidate {    hit: SourceHit,
    lexical_rank: Option<usize>,
    /// Lexical coverage (`distinct_terms / query_terms`), 0 without a lexical hit.
    coverage: f64,
    semantic_rank: Option<usize>,
    /// Record-level semantic cosine (0 when the record has no vector).
    similarity: f64,
    source: String,
    turn: u32,
    /// Bonus applied when several distinct units of the same turn were matched.
    turn_boost: f64,
}

/// Semantic-dominant score: the record cosine plus a bounded lexical bonus and
/// the tiny turn tie-break. Callers without a vector contribute `similarity=0`.
fn fused_score(similarity: f64, coverage: f64, turn_boost: f64) -> f64 {
    similarity + LEXICAL_BONUS * coverage + turn_boost
}

impl FusedCandidate {
    fn score(&self) -> f64 {
        // Round-4 fusion: the semantic cosine is the ranking source; lexical
        // coverage is a bounded bonus. A lexical branch can therefore never
        // outrank a clearly better semantic target just because it shares more
        // surface terms.
        fused_score(self.similarity, self.coverage, self.turn_boost)
    }
}

pub fn search(request: &ArchiveRequest<'_>) -> SourceOutcome {
    let source = request.source;
    let started = Instant::now();
    let archive = match open(request.read_root) {
        Ok(archive) => archive,
        Err(failure) => return failure_outcome(source, &failure),
    };
    let manifests = match archive.manifests() {
        Ok(manifests) => manifests,
        Err(error) => return archive_error_outcome(source, &error),
    };
    let units = load_units(&manifests);
    let access = object_access(&manifests);
    let sessions = session_map(request.read_root, request.domain.effective);
    let mut counters = Counters::new();
    counters.records = units.len() as u64;

    // Semantic admission: the generation must match the configured identity and
    // the effective access domain before a single vector is read. A degradation
    // (missing index, another model, wrong domain, model unavailable) never
    // changes the lexical path.
    let mut semantic_runtime: Option<SemanticRuntime> = None;
    let mut semantic_report: Option<SemanticReport> = None;
    if let Some(plan) = request.embedding {
        let loaded = semantic_index::load(request.read_root, request.domain.effective, plan);
        let mut report = SemanticReport {
            status: "degraded",
            reason: loaded.reason.clone(),
            generation_key: plan.generation_key(),
            generation: loaded.generation,
            dimension: loaded.dimension,
            units: loaded.units,
            candidates: 0,
            fused: false,
        };
        if loaded.available() {
            match plan.embed_one(request.query) {
                Ok(query) => {
                    let query_norm = semantic_index::vector_norm(&query);
                    report.status = "available";
                    report.reason = None;
                    semantic_runtime = Some(SemanticRuntime {
                        index: loaded,
                        query,
                        query_norm,
                        candidates: BTreeMap::new(),
                    });
                }
                Err(error) => report.reason = Some(error.reason),
            }
        }
        semantic_report = Some(report);
    }

    // P4 report: present whenever the request asked for expansion, so an empty
    // Archive still answers explicitly instead of omitting the key.
    let mut related_report: Option<RelatedReport> = request.related_hops.map(|hops| RelatedReport {
        hops,
        nodes: 0,
        denied: 0,
        sidechain: 0,
        truncated: 0,
    });

    if units.is_empty() {
        return SourceOutcome {
            source_id: source.source_id.clone(),
            status: status::EMPTY,
            code: None,
            detail: Some(detail(
                &counters,
                &sessions,
                semantic_report.as_ref(),
                0,
                Some("archive_has_no_records"),
                None,
                related_report.as_ref(),
            )),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: None,
            side_effects: Vec::new(),
            violations: Vec::new(),
        };
    }

    let root_sha = root_sha256(&source.root);
    let mut objects: BTreeMap<String, ObjectLoad> = BTreeMap::new();
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut hits: Vec<SourceHit> = Vec::new();
    let mut turn_by_hit: BTreeMap<String, (String, u32)> = BTreeMap::new();
    let mut turns_by_source: BTreeMap<String, u32> = BTreeMap::new();
    let mut consumed = 0u64;
    let mut stop: Option<StopReason> = None;
    let mut gated_out = 0usize;
    // P4: `related` is opt-in. When it is off nothing below allocates, so the
    // response stays byte-identical to P3.
    let mut graphs: BTreeMap<String, SourceGraph> = BTreeMap::new();
    let mut projections: BTreeMap<usize, String> = BTreeMap::new();
    let mut hit_origin: BTreeMap<String, usize> = BTreeMap::new();

    for (unit_index, unit) in units.iter().enumerate() {
        // Record-level predicate: an empty label set is a missing permission
        // mapping (fail-closed, reported as an anomaly), a non-empty set that
        // does not contain the effective domain is out of scope for this domain.
        let object_labels = match access.get(&unit.record.object) {
            Some(labels) => labels,
            None => {
                // The object has no readable committed reference: an orphan or an
                // object whose references have no access mapping.
                counters.labels_missing += 1;
                continue;
            }
        };
        if unit.record.access.is_empty() {
            counters.labels_missing += 1;
            continue;
        }
        match classify_access(
            &unit.record.access,
            request.domain.effective,
            request.domain.explicit,
        ) {
            AccessDecision::Readable => {}
            AccessDecision::Denied(_) => {
                counters.denied += 1;
                continue;
            }
            AccessDecision::Unreachable(_) => {
                counters.unreachable += 1;
                continue;
            }
        }
        if !object_labels
            .iter()
            .any(|label| label == request.domain.effective)
        {
            // The shared object is referenced by another capture with a stricter
            // label set; the strictest intersection still denies it.
            counters.denied += 1;
            continue;
        }
        if let Some(wanted) = request.session_id {
            match sessions.session_for(&unit.source) {
                Some(session) if session == wanted => {}
                _ => {
                    // Unknown or different derived mapping: never guessed.
                    counters.unmapped += 1;
                    continue;
                }
            }
        } else if sessions.session_for(&unit.source).is_none() {
            counters.unmapped += 1;
        }
        if consumed >= request.budget.max_bytes_per_source {
            stop = Some(StopReason::Bytes);
            break;
        }
        if started.elapsed().as_millis() as u64 >= request.budget.max_elapsed_ms_per_source {
            stop = Some(StopReason::Time);
            break;
        }

        let Some(rel_path) = object_rel_path(&unit.record.object) else {
            counters.corrupt += 1;
            continue;
        };
        let load = objects.entry(unit.record.object.clone()).or_insert_with(|| {
            match archive.read_object_bytes(&unit.record.object) {
                Ok(bytes) => ObjectLoad::Bytes(bytes),
                Err(ArchiveError::NotFound(_)) => ObjectLoad::Missing,
                Err(_) => ObjectLoad::Corrupt,
            }
        });
        let bytes = match load {
            ObjectLoad::Bytes(bytes) => bytes,
            ObjectLoad::Missing => {
                counters.missing += 1;
                continue;
            }
            ObjectLoad::Corrupt => {
                counters.corrupt += 1;
                continue;
            }
        };
        let start = unit.record.offset as usize;
        let end = match start.checked_add(unit.record.len as usize) {
            Some(end) if end <= bytes.len() => end,
            _ => {
                counters.corrupt += 1;
                continue;
            }
        };
        let slice = &bytes[start..end];
        if hex_sha256(slice) != unit.record.sha256 {
            counters.corrupt += 1;
            continue;
        }
        consumed = consumed.saturating_add(unit.record.len);
        let Some(projection) = project_record(slice) else {
            counters.unprojectable += 1;
            continue;
        };
        // `searched` counts records that reached the lexical comparison; an
        // unprojectable record is reported separately, never as "searched".
        counters.searched += 1;
        // Turn index for this record (per physical source). It mirrors the
        // evaluation contract and is used only as a ranking signal.
        let turn = {
            let current = turns_by_source.entry(unit.source.clone()).or_insert(0);
            if crate::recall::semantic::units::is_user_turn_record(&unit.provider, &projection) {
                *current += 1;
            }
            *current
        };
        // P4 graph: built only when the request asked for `related`. Records that
        // are sidechain transcripts or that never project are not pushed, so the
        // missing unit index proves "no edge" instead of guessing one.
        if request.related_hops.is_some() {
            let graph_entry = graphs.entry(unit.source.clone()).or_default();
            if graph::is_subagent_session_record(&unit.provider, &projection) {
                graph_entry.subagent = true;
            }
            if graph::is_sidechain_record(&unit.provider, &projection) {
                graph_entry.note_sidechain(unit_index);
            } else {
                // Metadata-only records (`session_meta`, `turn_context`, …) keep
                // the chain contiguous but are never selected as nodes.
                let evidence =
                    !crate::recall::semantic::units::extract(&unit.provider, &projection)
                        .is_empty();
                graph_entry.push(unit_index, &unit.provider, &projection, evidence);
                projections.insert(unit_index, projection.clone());
            }
        }
        let projection_bytes = projection.as_bytes();
        let range = TextRange::from_span(projection_bytes, 0, projection_bytes.len());
        let reference = HitRef {
            source_id: source.source_id.clone(),
            source_kind: SOURCE_KIND_ARCHIVE.to_string(),
            root_sha256: root_sha.clone(),
            rel_path,
            schema_gen: SCHEMA_ARCHIVE_RECORD_V1.to_string(),
            locator: Locator::archive(&unit.record.sha256, 0),
            fingerprint: Fingerprint {
                range_sha256: sha256_hex(projection_bytes),
                file_sha256: None,
                row_sha256: None,
            },
            text_range: range,
        };
        // Identical content (same object, record, range and source) is one hit
        // even when two physical sources captured it.
        let id = hit_id(&reference);
        let folded = fold_ascii(&projection);
        if let Some(found) = lexical_match(&folded, request.terms) {
            if emitted.insert(id.clone()) {
                hit_origin.insert(id.clone(), unit_index);
                let (snippet_text, snippet_truncated) = snippet(
                    &projection,
                    found.first_byte,
                    found.first_term_len,
                    request.snippet_bytes,
                );
                turn_by_hit.insert(id.clone(), (unit.source.clone(), turn));
                hits.push(SourceHit {
                    hit: reference.clone(),
                    title: title_for(unit),
                    snippet: snippet_text,
                    snippet_truncated,
                    score: None,
                    score_kind: SCORE_KIND_NONE,
                    rank_basis: RANK_BASIS_LEXICAL_WINDOW,
                    distinct_terms: found.distinct_terms,
                    first_byte: found.first_byte,
                    sort_time: unit.captured_at_ms,
                    sort_id: format!("{}:{}", unit.source, unit.record.sha256),
                    related: Vec::new(),
                        exact: None,
                });
            }
        }

        // Semantic candidates are generated only for records that already passed
        // the object/record access predicates and the session filter above. A
        // denied unit is therefore never scored, even if it is the most similar
        // vector in the generation.
        if let Some(runtime) = semantic_runtime.as_mut() {
            for piece in extract_units(&unit.provider, &projection) {
                let normalized = normalize_unit_text(&piece.text);
                if normalized.is_empty() {
                    continue;
                }
                let sha = unit_sha256(&normalized, &piece.unit_type);
                let Some(row) = runtime.index.row(&sha) else {
                    continue;
                };
                let similarity = row.similarity(&runtime.query, runtime.query_norm);
                if runtime
                    .candidates
                    .get(&id)
                    .map(|candidate| candidate.similarity >= similarity)
                    .unwrap_or(false)
                {
                    continue;
                }
                let (snippet_text, snippet_truncated) =
                    snippet(&piece.text, 0, 0, request.snippet_bytes);
                turn_by_hit.insert(id.clone(), (unit.source.clone(), turn));
                hit_origin.insert(id.clone(), unit_index);
                runtime.candidates.insert(
                    id.clone(),
                    SemanticCandidate {
                        similarity,
                        source: unit.source.clone(),
                        turn,
                        hit: SourceHit {
                            hit: reference.clone(),
                            title: title_for(unit),
                            snippet: snippet_text,
                            snippet_truncated,
                            score: Some(similarity),
                            score_kind: SCORE_KIND_SEMANTIC_BONUS,
                            rank_basis: RANK_BASIS_SEMANTIC_BONUS,
                            distinct_terms: 0,
                            first_byte: 0,
                            sort_time: unit.captured_at_ms,
                            sort_id: format!("{}:{}", unit.source, unit.record.sha256),
                            related: Vec::new(),
                        exact: None,
                        },
                    },
                );
            }
        }
    }

    rank_hits(&mut hits);

    // Round-4 fusion: candidates are generated exactly as P1/round 1 (no
    // semantic floor, no lexical coverage gate). When the semantic path is
    // available the rank score is `semantic_cosine + LEXICAL_BONUS * coverage`
    // (plus the turn tie-break): semantic is the ranking source and lexical
    // evidence is bounded. A post-fusion gate only runs when the semantic path
    // is available, and it never changes the order of the surviving hits.
    let mut semantic_items: Vec<(String, f64, SourceHit, String, u32)> = Vec::new();
    let mut semantic_by_record: BTreeMap<String, f64> = BTreeMap::new();
    let mut semantic_by_turn: BTreeMap<(String, u32), Vec<f64>> = BTreeMap::new();
    if let Some(runtime) = semantic_runtime {
        for (id, candidate) in &runtime.candidates {
            semantic_by_record.insert(id.clone(), candidate.similarity);
            semantic_by_turn
                .entry((candidate.source.clone(), candidate.turn))
                .or_default()
                .push(candidate.similarity);
        }
        for (id, candidate) in runtime.candidates {
            semantic_items.push((
                id,
                candidate.similarity,
                candidate.hit,
                candidate.source,
                candidate.turn,
            ));
        }
        semantic_items.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.2.sort_id.cmp(&b.2.sort_id))
        });
        semantic_items.truncate(SEMANTIC_MAX_CANDIDATES);
        if let Some(report) = semantic_report.as_mut() {
            report.candidates = semantic_items.len();
            report.fused = true;
        }
        let mut entries: Vec<FusedCandidate> = Vec::new();
        let mut positions: BTreeMap<String, usize> = BTreeMap::new();
        let query_terms = request.terms.len().max(1);
        for (rank, hit) in hits.iter().enumerate() {
            let id = hit_id(&hit.hit);
            positions.insert(id.clone(), entries.len());
            let (source, turn) = turn_by_hit
                .get(&id)
                .cloned()
                .unwrap_or_else(|| (String::new(), 0));
            let coverage = lexical_coverage(hit.distinct_terms, query_terms);
            // A lexical hit may not be in the top-N semantic candidate list but
            // still has a vector: use its record cosine, and fall back to 0 only
            // when the record has no vector at all.
            let similarity = semantic_by_record.get(&id).copied().unwrap_or(0.0);
            entries.push(FusedCandidate {
                hit: hit.clone(),
                lexical_rank: Some(rank),
                coverage,
                semantic_rank: None,
                similarity,
                source,
                turn,
                turn_boost: 0.0,
            });
        }
        for (rank, (id, similarity, hit, source, turn)) in
            semantic_items.into_iter().enumerate()
        {
            match positions.get(&id) {
                Some(index) => {
                    entries[*index].semantic_rank = Some(rank);
                    entries[*index].similarity = similarity;
                }
                None => {
                    positions.insert(id, entries.len());
                    entries.push(FusedCandidate {
                        hit,
                        lexical_rank: None,
                        coverage: 0.0,
                        semantic_rank: Some(rank),
                        similarity,
                        source,
                        turn,
                        turn_boost: 0.0,
                    });
                }
            }
        }
        // Turn boost is a tie-break only: it changes the fused score of existing
        // candidates and never creates or removes one.
        let mut turn_counts: BTreeMap<(String, u32), usize> = BTreeMap::new();
        for entry in &entries {
            *turn_counts
                .entry((entry.source.clone(), entry.turn))
                .or_insert(0) += 1;
        }
        for entry in &mut entries {
            let count = turn_counts
                .get(&(entry.source.clone(), entry.turn))
                .copied()
                .unwrap_or(1);
            if count > 1 {
                entry.turn_boost = TURN_AGGREGATION_BONUS * ((count - 1).min(3) as f64);
            }
        }
        entries.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.lexical_rank.cmp(&b.lexical_rank))
                .then_with(|| a.semantic_rank.cmp(&b.semantic_rank))
                .then_with(|| a.hit.sort_id.cmp(&b.hit.sort_id))
        });
        hits = entries
            .into_iter()
            .map(|mut entry| {
                entry.hit.score = Some(entry.score());
                entry.hit.score_kind = SCORE_KIND_SEMANTIC_BONUS;
                entry.hit.rank_basis = RANK_BASIS_SEMANTIC_BONUS;
                if entry.lexical_rank.is_none() {
                    entry.hit.distinct_terms = 0;
                    entry.hit.first_byte = 0;
                }
                entry.hit
            })
            .collect();

        // Acceptance gate (semantic available only). Keep a hit when:
        // a) its record/turn has cosine >= 0.64;
        // b) lexical coverage >= 0.34 with >= 2 terms (single-term queries exempt);
        // c) its turn has >= 2 records >= 0.50 and turn max >= 0.60.
        // `retain` preserves the relative order of surviving hits.
        let before = hits.len();
        hits.retain(|hit| {
            let id = hit_id(&hit.hit);
            let (source, turn) = turn_by_hit
                .get(&id)
                .cloned()
                .unwrap_or_else(|| (String::new(), 0));
            let record_similarity = semantic_by_record.get(&id).copied().unwrap_or(f64::MIN);
            let turn_similarities = semantic_by_turn
                .get(&(source.clone(), turn))
                .cloned()
                .unwrap_or_default();
            let turn_max = turn_similarities
                .iter()
                .copied()
                .fold(f64::MIN, f64::max);
            let support = turn_similarities
                .iter()
                .filter(|similarity| **similarity >= SEMANTIC_SUPPORT_SIMILARITY)
                .count();
            let rule_a = record_similarity >= SEMANTIC_ACCEPT_SIMILARITY
                || turn_max >= SEMANTIC_ACCEPT_SIMILARITY;
            let rule_b = lexical_accepts(hit.distinct_terms, request.terms.len());
            let rule_c = support >= SEMANTIC_SUPPORT_UNITS
                && turn_max >= SEMANTIC_TURN_MAX_SIMILARITY;
            rule_a || rule_b || rule_c
        });
        gated_out = before.saturating_sub(hits.len());
    }

    // ---- P4: bounded `next`-edge expansion ---------------------------------
    //
    // Runs strictly after access filtering and after the acceptance gate. It is
    // additive: the main result order, scores, `gated_out` and `source_status`
    // are already fixed. A denied or invisible record is never a node, and a
    // subagent/sidechain source is never expanded at all; both are counted in
    // `detail.archive.related`.
    if let Some(report) = related_report.as_mut() {
        let hops = report.hops;
        let related_started = Instant::now();
        let mut per_source: BTreeMap<String, usize> = BTreeMap::new();
        for hit in &mut hits {
            let id = hit_id(&hit.hit);
            let Some(unit_index) = hit_origin.get(&id).copied() else {
                continue;
            };
            let Some(unit) = units.get(unit_index) else {
                continue;
            };
            let Some(source_graph) = graphs.get(&unit.source) else {
                report.denied += 1;
                continue;
            };
            let (nodes, counters) = source_graph.related_nodes(unit_index, hops);
            report.denied += counters.denied;
            report.sidechain += counters.sidechain;
            report.truncated += counters.truncated;
            let used = per_source.entry(unit.source.clone()).or_insert(0);
            for node in nodes {
                if *used >= graph::RELATED_NODES_PER_SOURCE
                    || related_started.elapsed().as_millis() as u64
                        >= graph::RELATED_MAX_ELAPSED_MS
                {
                    report.truncated += 1;
                    continue;
                }
                let Some(target) = units.get(node.unit) else {
                    report.denied += 1;
                    continue;
                };
                let Some(text) = projections.get(&node.unit) else {
                    report.denied += 1;
                    continue;
                };
                let Some(rel_path) = object_rel_path(&target.record.object) else {
                    report.denied += 1;
                    continue;
                };
                let bytes = text.as_bytes();
                let reference = HitRef {
                    source_id: source.source_id.clone(),
                    source_kind: SOURCE_KIND_ARCHIVE.to_string(),
                    root_sha256: root_sha.clone(),
                    rel_path,
                    schema_gen: SCHEMA_ARCHIVE_RECORD_V1.to_string(),
                    locator: Locator::archive(&target.record.sha256, 0),
                    fingerprint: Fingerprint {
                        range_sha256: sha256_hex(bytes),
                        file_sha256: None,
                        row_sha256: None,
                    },
                    text_range: TextRange::from_span(bytes, 0, bytes.len()),
                };
                hit.related.push(RelatedItem {
                    hit_id: hit_id(&reference),
                    title: title_for(target),
                    reference,
                    edge: graph::EDGE_NEXT,
                    direction: node.direction,
                    hops: node.hops,
                    turn: node.turn,
                });
                *used += 1;
                report.nodes += 1;
            }
        }
    }

    let mut reason: Option<&'static str> = None;
    let (status_value, code, message) = if counters.searched == 0
        && counters.denied + counters.unreachable > 0
    {
        reason = Some("archive_records_not_readable");
        let unreachable = counters.unreachable > 0;
        (
            status::UNAVAILABLE,
            Some(if unreachable {
                codes::DOMAIN_UNREACHABLE
            } else {
                codes::DOMAIN_DENIED
            }),
            Some(format!(
                "no Archive record is readable for domain {:?} (denied {}, unreachable {})",
                request.domain.effective, counters.denied, counters.unreachable
            )),
        )
    } else if counters.searched == 0 && counters.labels_missing > 0 {
        // Empty access labels are a missing permission mapping: fail closed and
        // report it instead of a healthy empty result.
        reason = Some("archive_access_labels_missing");
        (
            status::UNAVAILABLE,
            Some(codes::DOMAIN_DENIED),
            Some(format!(
                "{} Archive record(s) have no readable access mapping (fail-closed)",
                counters.labels_missing
            )),
        )
    } else if counters.searched == 0 && counters.missing > 0 {
        reason = Some("archive_object_missing");
        (
            status::UNAVAILABLE,
            Some(codes::DB_UNAVAILABLE),
            Some(format!(
                "{} Archive record object(s) are missing from the object store",
                counters.missing
            )),
        )
    } else if counters.searched == 0 && counters.corrupt > 0 {
        reason = Some("archive_object_unreadable");
        (
            status::UNAVAILABLE,
            Some(codes::DB_CORRUPT),
            Some(format!(
                "{} Archive record object(s) failed integrity checks",
                counters.corrupt
            )),
        )
    } else if counters.unprojectable > 0 {
        reason = Some("archive_record_unprojectable");
        (
            status::PARTIAL,
            Some(codes::CONTENT_UNRESOLVED),
            Some(format!(
                "{} Archive record(s) have no UTF-8 text projection",
                counters.unprojectable
            )),
        )
    } else if counters.searched == 0 && counters.unmapped > 0 && request.session_id.is_some() {
        reason = Some("archive_session_unmapped");
        (
            status::EMPTY,
            None,
            Some(format!(
                "session_id {:?} matches no derived Archive session mapping (unmapped is never guessed)",
                request.session_id.unwrap_or_default()
            )),
        )
    } else if let Some(reason) = stop {
        (
            status::TRUNCATED,
            Some(match reason {
                StopReason::Bytes => codes::BUDGET_BYTES,
                StopReason::Time => codes::BUDGET_TIME,
            }),
            Some("archive source budget exhausted".to_string()),
        )
    } else if counters.labels_missing > 0 {
        // Some readable records were searched, but records without a permission
        // mapping exist: expose the anomaly instead of presenting a clean
        // result. Out-of-scope (other-domain) records are counted separately and
        // do not degrade a normal domain-scoped search.
        reason = Some("archive_access_labels_missing");
        (
            status::PARTIAL,
            Some(codes::RECORD_SKIPPED),
            Some(format!(
                "{} Archive record(s) have no readable access mapping (fail-closed)",
                counters.labels_missing
            )),
        )
    } else if hits.is_empty() {
        (status::EMPTY, None, None)
    } else {
        (status::OK, None, None)
    };

    let budget = stop.map(|reason| {
        let mut budget = Map::new();
        match reason {
            StopReason::Bytes => {
                budget.insert("reason".into(), Value::String(codes::BUDGET_BYTES.to_string()));
                budget.insert("consumed_bytes".into(), Value::from(consumed));
            }
            StopReason::Time => {
                budget.insert("reason".into(), Value::String(codes::BUDGET_TIME.to_string()));
                budget.insert(
                    "elapsed_ms".into(),
                    Value::from(started.elapsed().as_millis() as u64),
                );
            }
        }
        Value::Object(budget)
    });

    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code,
        detail: Some(detail(
            &counters,
            &sessions,
            semantic_report.as_ref(),
            gated_out,
            reason,
            budget,
            related_report.as_ref(),
        )),
        hits,
        warnings: Vec::new(),
        error_message: message,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

fn title_for(unit: &Unit) -> String {
    let name = Path::new(&unit.source)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| unit.source.clone());
    let provider = if unit.provider.is_empty() {
        "archive"
    } else {
        unit.provider.as_str()
    };
    match unit.record.source_line {
        Some(line) => format!("{provider} {name} line {line}"),
        None => format!("{provider} {name} record {}", unit.record.seq),
    }
}

fn detail(
    counters: &Counters,
    sessions: &SessionMap,
    semantic: Option<&SemanticReport>,
    gated_out: usize,
    reason: Option<&'static str>,
    budget: Option<Value>,
    related: Option<&RelatedReport>,
) -> Value {
    let mut archive = Map::new();
    archive.insert("records".into(), Value::from(counters.records));
    archive.insert("searched".into(), Value::from(counters.searched));
    archive.insert("denied".into(), Value::from(counters.denied));
    archive.insert("unreachable".into(), Value::from(counters.unreachable));
    archive.insert("unmapped".into(), Value::from(counters.unmapped));
    archive.insert(
        "thresholdsVersion".into(),
        Value::String(ARCHIVE_RELEVANCE_VERSION.to_string()),
    );
    archive.insert("session_mapping".into(), Value::String(sessions.status.to_string()));
    if let Some(generation) = sessions.generation {
        archive.insert("derived_generation".into(), Value::from(generation));
    }
    if counters.missing > 0 {
        archive.insert("missing_objects".into(), Value::from(counters.missing));
    }
    if counters.corrupt > 0 {
        archive.insert("corrupt_objects".into(), Value::from(counters.corrupt));
    }
    if counters.labels_missing > 0 {
        archive.insert("labels_missing".into(), Value::from(counters.labels_missing));
    }
    if gated_out > 0 {
        archive.insert("gated_out".into(), Value::from(gated_out));
    }

    if semantic.is_some() {
        archive.insert("scoreFusion".into(), Value::String(SCORE_FUSION.to_string()));
        archive.insert("lexicalBonus".into(), Value::from(LEXICAL_BONUS));
    }
    if let Some(related) = related {
        // Only present when the request asked for `related_hops`; the P3
        // response therefore keeps exactly its old key set.
        archive.insert("related".into(), related_detail(related));
    }
    let mut out = Map::new();
    out.insert("archive".into(), Value::Object(archive));
    if let Some(report) = semantic {
        let mut block = Map::new();
        block.insert("status".into(), Value::String(report.status.to_string()));
        if let Some(reason) = &report.reason {
            block.insert("reason".into(), Value::String(reason.clone()));
        }
        block.insert(
            "generationKey".into(),
            Value::String(report.generation_key.clone()),
        );
        if let Some(generation) = report.generation {
            block.insert("generation".into(), Value::from(generation));
        }
        block.insert("dimension".into(), Value::from(report.dimension));
        block.insert("units".into(), Value::from(report.units));
        block.insert("candidates".into(), Value::from(report.candidates));
        block.insert(
            "fusion".into(),
            Value::String(if report.fused { SCORE_FUSION } else { "none" }.to_string()),
        );
        block.insert("scoreFusion".into(), Value::String(SCORE_FUSION.to_string()));
        block.insert("lexicalBonus".into(), Value::from(LEXICAL_BONUS));
        block.insert(
            "thresholdsVersion".into(),
            Value::String(ARCHIVE_RELEVANCE_VERSION.to_string()),
        );
        block.insert(
            "thresholds".into(),
            serde_json::json!({
                "semanticAcceptSimilarity": SEMANTIC_ACCEPT_SIMILARITY,
                "semanticSupportSimilarity": SEMANTIC_SUPPORT_SIMILARITY,
                "semanticSupportUnits": SEMANTIC_SUPPORT_UNITS,
                "semanticTurnMaxSimilarity": SEMANTIC_TURN_MAX_SIMILARITY,
                "lexicalAcceptCoverage": LEXICAL_ACCEPT_COVERAGE,
                "lexicalAcceptDistinct": LEXICAL_ACCEPT_DISTINCT,
                "turnAggregationBonus": TURN_AGGREGATION_BONUS,
            }),
        );
        out.insert("semantic".into(), Value::Object(block));
    }
    if counters.unprojectable > 0 {
        out.insert(
            "CONTENT_UNRESOLVED".into(),
            Value::from(counters.unprojectable),
        );
    }
    if let Some(budget) = budget {
        out.insert("budget".into(), budget);
    }
    if let Some(reason) = reason {
        out.insert("reason".into(), Value::String(reason.to_string()));
    }
    Value::Object(out)
}

fn related_detail(report: &RelatedReport) -> Value {
    let mut block = Map::new();
    block.insert("edge".into(), Value::String(graph::EDGE_NEXT.to_string()));
    block.insert("hops".into(), Value::from(report.hops as u64));
    block.insert("nodes".into(), Value::from(report.nodes as u64));
    block.insert("denied".into(), Value::from(report.denied as u64));
    block.insert("sidechain".into(), Value::from(report.sidechain as u64));
    block.insert("truncated".into(), Value::from(report.truncated as u64));
    block.insert(
        "limits".into(),
        serde_json::json!({
            "maxHops": graph::RELATED_MAX_HOPS,
            "maxNodesPerHit": graph::RELATED_NODES_PER_HIT,
            "maxNodesPerSource": graph::RELATED_NODES_PER_SOURCE,
            "maxElapsedMs": graph::RELATED_MAX_ELAPSED_MS,
        }),
    );
    Value::Object(block)
}

fn failure_outcome(source: &SourceConfig, failure: &ArchiveFailure) -> SourceOutcome {
    SourceOutcome {
        source_id: source.source_id.clone(),
        status: failure.status,
        code: Some(failure.code),
        detail: Some(serde_json::json!({ "reason": failure.reason })),
        hits: Vec::new(),
        warnings: Vec::new(),
        error_message: Some(failure.message.clone()),
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

fn archive_error_outcome(source: &SourceConfig, error: &ArchiveError) -> SourceOutcome {
    let code = match error {
        ArchiveError::AccessDenied(_) => codes::DOMAIN_DENIED,
        ArchiveError::NotFound(_) => codes::DB_UNAVAILABLE,
        ArchiveError::Integrity(_) | ArchiveError::Corrupt(_) => codes::DB_CORRUPT,
        _ => codes::DB_UNAVAILABLE,
    };
    SourceOutcome {
        source_id: source.source_id.clone(),
        status: crate::recall::contract::status_for_code(code),
        code: Some(code),
        detail: Some(serde_json::json!({ "reason": "archive_unreadable" })),
        hits: Vec::new(),
        warnings: Vec::new(),
        error_message: Some(error.to_string()),
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_addresses_round_trip_and_reject_escapes() {
        let sha = "ab".repeat(32);
        let rel = object_rel_path(&sha).expect("valid address");
        assert_eq!(rel, format!("archive/objects/ab/ab/{sha}"));
        assert_eq!(object_sha_from_rel_path(&rel), Some(sha.clone()));
        for bad in [
            "archive/objects/aa/bb/cc",
            "archive/objects/ab/ab/../x",
            "objects/ab/ab/x",
            "archive/objects/ab/ab",
            "archive/objects/ab/cd/x",
            "",
        ] {
            assert!(object_sha_from_rel_path(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn projection_is_the_decoded_record_and_never_a_guess() {
        assert_eq!(
            project_record(b"{\"text\":\"needle\"}\n"),
            Some("{\"text\":\"needle\"}".to_string())
        );
        assert_eq!(project_record(b"   \n"), None);
        assert_eq!(project_record(&[0xff, 0xfe]), None);
    }

    #[test]
    fn semantic_cosine_dominates_the_bounded_lexical_bonus() {
        // A cross-lingual semantic target (~0.7) can never be outranked by a
        // lexical-only candidate, whatever its coverage, because the bonus is
        // bounded by LEXICAL_BONUS (at most 0.2 in the calibration grid).
        let semantic = fused_score(0.70, 0.0, 0.0);
        let lexical_only = fused_score(0.0, 1.0, 0.0);
        assert!(semantic > lexical_only, "{semantic} vs {lexical_only}");
        assert!(LEXICAL_BONUS <= 0.2);
        // When semantic similarity is equal, coverage can only add the bonus.
        let plain = fused_score(0.50, 0.0, 0.0);
        let covered = fused_score(0.50, 1.0, 0.0);
        assert!(covered >= plain);
        assert!((covered - plain - LEXICAL_BONUS).abs() < 1e-12);
        // The turn aggregation bonus stays a tie-break, far below a cosine step.
        let boosted = fused_score(0.50, 0.0, TURN_AGGREGATION_BONUS * 3.0);
        assert!((boosted - plain - 0.003).abs() < 1e-12);
        assert!(TURN_AGGREGATION_BONUS * 3.0 < 0.01);
    }

    #[test]
    fn lexical_acceptance_rule_matches_the_gate_boundaries() {
        // One-term query: any matched term is enough.
        assert!(lexical_accepts(1, 1));
        assert!(!lexical_accepts(0, 1));
        // Multi-term: >= 2 terms and >= 34% coverage.
        assert!(!lexical_accepts(1, 8));
        assert!(!lexical_accepts(2, 10));
        assert!(lexical_accepts(4, 10));
        assert!(lexical_accepts(2, 2));
    }

    #[test]
    fn locator_carries_only_record_and_projection_offset() {
        let locator = Locator::archive("ab", 0);
        assert_eq!(
            locator.compact_json(),
            "{\"record_sha256\":\"ab\",\"projection_offset\":0}"
        );
        assert!(!locator.compact_json().contains("capture"));
    }
}

