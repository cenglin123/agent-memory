//! Stage-2 S5 archive fallback: sessions that are gone from a live source keep
//! their units in that source's index, built from the committed evidence
//! ("live first, archive fills the gaps").
//!
//! # Rules
//!
//! * `index` for a live source looks up the archive source in the same
//!   configuration (`kind = "archive"`, enabled or not — the same rule
//!   `capture-all` uses for its runtime root). No archive source: the build is
//!   byte-for-byte the pre-S5 build.
//! * The archived-session inventory is derived from manifests and objects, one
//!   carrier group at a time, and cached under `<cache_root>/archive-inventory/`
//!   keyed by content fingerprints. The manifest set is parsed at most once per
//!   source build and that one snapshot serves the inventory and every carrier
//!   rebuild of the run; object decompression only runs for groups whose
//!   fingerprint is new.
//! * `wanted = archived sessions − live sessions`. Wanted sessions get units
//!   from the **existing** live-source projectors (the JSONL record scanners
//!   and the OpenCode V2 unit builder), marked `origin = archive`, written into
//!   the same `fts.sqlite`, and registered in `archive_sessions`.
//! * A session that reappears in the live source loses its archive rows
//!   (removal is origin-aware, so live rows of the same session survive) and
//!   the registry entry is dropped: one session, one set of rows.
//! * `memory_read` follows the `archive_fallback` locator marker: JSONL refs
//!   rebuild their carrier from the committed capture generation; OpenCode refs
//!   materialize the session's whitelisted V2 rows and reuse the live V2
//!   reader. Range/row fingerprint checks are the live ones. Readback never
//!   writes the cache root: a missing or stale SQLite row mirror is
//!   materialized into a temporary file that is removed afterwards.
//!
//! # Honest limits
//!
//! * Archive rows are refreshed only when a session is newly wanted, when its
//!   index rows vanished, or on a full rebuild. A wanted session's evidence is
//!   frozen (its live file is gone, or a compressed carrier that cannot grow),
//!   so skipping known-present sessions is safe.
//! * The SQLite row mirror reproduces the archived `session_v2` +
//!   `session_message` state per baseline generation and advances incrementally
//!   with committed delta batches; its size approaches the whitelisted V2
//!   payload of the source and is deletable.
//! * JSONL carriers are rebuilt from the current capture generation of their
//!   manifest group; a group with a gap in its committed byte ranges is skipped
//!   and counted, never guessed.

use super::build::{codex_marker_from_reader, codex_marker_from_value, scan_jsonl_reader, Writer};
use super::{FallbackMeta, ORIGIN_ARCHIVE};
use crate::archive::{Archive, Manifest};
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{codes, RecallError, SCHEMA_DSH_JSONL, SCHEMA_SQLITE_V2};
use crate::recall::fulltext::build::FallbackIdentity;
use crate::recall::hash::sha256_hex;
use crate::recall::port::reader::rel_from_root;
use crate::recall::port::PartData;
use crate::recall::providers::archive::{self as archive_provider};
use crate::recall::providers::dsh;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::OnceCell;
use std::sync::Arc;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// Cache schema of the composed inventory (`current.json`).
pub const INVENTORY_SCHEMA: &str = "agent-memory/archive-inventory@1";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The archive runtime root configured next to the live sources (the same
/// selection rule `capture-all` uses: the first `kind = archive` source,
/// enabled or not).
pub fn configured_archive_root(config: &RecallConfig) -> Option<String> {
    config
        .sources
        .iter()
        .find(|source| source.kind == SourceKind::Archive)
        .map(|source| source.root.clone())
}

// ---------------------------------------------------------------------------
// Inventory (what is archived, per carrier group)
// ---------------------------------------------------------------------------

/// One archived physical carrier (a JSONL file or a SQLite source).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CarrierGroup {
    /// Manifest `source.path` (the collector's stable source key).
    pub path: String,
    /// `claude` | `codex` | `opencode` (the manifest provider).
    pub provider: String,
    /// `jsonl_file` | `compressed_jsonl_file` | `sqlite` | `dsh_jsonl`.
    pub kind: String,
    /// DSH only: the carrier relative path inside the live root. The carrier
    /// file name changes on a format-generation switch while `path` (the
    /// session directory) stays stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    /// Content fingerprint of the group's selected manifests: a changed group
    /// re-derives its session list, everything else is served from cache.
    pub fingerprint: String,
    /// Session ids the live builder would assign (same rules: record
    /// `sessionId` / first-line `session_meta` / `session_v2` rows).
    pub sessions: BTreeSet<String>,
    /// JSONL only: at least one record carried no session id of its own, so
    /// the live builder would fall back to the rel-path inference for those
    /// lines. The inferred id is resolved at apply time from the mapped rel.
    #[serde(default)]
    pub needs_infer: bool,
    /// SQLite only: baseline digest the session list was replayed from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub baseline_digest: String,
    /// SQLite only: highest committed capture seq included.
    #[serde(default)]
    pub capture_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InventoryFile {
    schema: String,
    rule_version: String,
    fingerprint: String,
    generated_at_ms: i64,
    groups: Vec<CarrierGroup>,
}

/// Manifest-set fingerprint: manifests are append-only and uniquely identified
/// by their `capture_id`, so the sorted id set identifies the evidence set
/// without parsing object bytes.
fn manifest_set_fingerprint_from_names(names: &[String]) -> String {
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    names.sort();
    names.dedup();
    sha256_hex(names.join("\n").as_bytes())
}

/// Content fingerprint of one carrier group's selected evidence (which
/// manifests carry it, which objects they reference).
fn group_fingerprint(selected: &[&Manifest]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for manifest in selected {
        let objects: Vec<&str> = manifest.objects.iter().map(|o| o.sha256.as_str()).collect();
        parts.push(format!(
            "{}|{}|{}|{}|{}|{}",
            manifest.capture_id,
            manifest.source.generation,
            manifest.source.range_start,
            manifest.source.carrier.as_deref().unwrap_or(""),
            manifest.records.len(),
            objects.join(",")
        ));
    }
    sha256_hex(parts.join("\n").as_bytes())
}

/// The evidence selection of the Archive provider, mirrored here: a whole-file
/// or compressed source is its **last** manifest, everything else its current
/// capture generation.
pub(crate) fn select_group_manifests<'a>(group: &[&'a Manifest]) -> Vec<&'a Manifest> {
    let last = group.last().expect("non-empty group");
    if last.source.kind == "jsonl_file" || last.source.kind == "dsh_jsonl" {
        // Incremental carriers capture append ranges: the current generation.
        let generation = group
            .iter()
            .map(|manifest| manifest.source.generation)
            .max()
            .unwrap_or(1);
        return group
            .iter()
            .filter(|manifest| manifest.source.generation == generation)
            .copied()
            .collect();
    }
    // whole-file carriers (including compressed rollouts): a full capture
    // replaces the previous one.
    vec![*last]
}

// ---------------------------------------------------------------------------
// Batch evidence reader
// ---------------------------------------------------------------------------

/// One committed-manifest snapshot for a run: every manifest is read and
/// parsed **at most once per source build**. The strictest per-object access
/// intersection is computed on the same pass, so N object reads cost one
/// manifest scan instead of N (the per-read handle re-scans all manifests for
/// every object - O(carriers x manifests) over a 4 000-carrier evidence store).
///
/// The snapshot is shared by the inventory derivation and by every carrier
/// rebuild of the run ([`FallbackContext::batch`]). Without that sharing, each
/// pending carrier re-parsed the whole manifest set, so a first round over K
/// carriers read `K x manifests` bytes.
///
/// Manifests are append-only and capture runs hold the single-writer lock, so
/// the snapshot is stable while the context lives; a reference committed mid-run
/// can only narrow access and is picked up by the next run.
pub(crate) struct EvidenceSnapshot {
    /// The archive's shared manifest snapshot (parsed once per change, never
    /// per object read).
    manifests: Arc<crate::archive::ManifestSnapshot>,
    access: BTreeMap<String, BTreeSet<String>>,
}

impl EvidenceSnapshot {
    fn load(archive: &Archive) -> Result<EvidenceSnapshot, String> {
        let manifests = archive.manifests().map_err(|error| error.to_string())?;
        let access = archive_provider::object_access(&manifests);
        Ok(EvidenceSnapshot { manifests, access })
    }
}

/// A reader over one [`EvidenceSnapshot`] and the archive that produced it.
pub(crate) struct EvidenceBatch<'a> {
    pub archive: &'a Archive,
    pub manifests: &'a [Manifest],
    access: &'a BTreeMap<String, BTreeSet<String>>,
    domain: &'a str,
}

impl<'a> EvidenceBatch<'a> {
    pub fn new(
        archive: &'a Archive,
        snapshot: &'a EvidenceSnapshot,
        domain: &'a str,
    ) -> EvidenceBatch<'a> {
        EvidenceBatch {
            archive,
            manifests: snapshot.manifests.as_slice(),
            access: &snapshot.access,
            domain,
        }
    }

    /// The same fail-closed predicate as `ReadHandle::read_object` (strictest
    /// intersection over every committed reference; empty or unreferenced
    /// deny), evaluated against the snapshot.
    pub fn read_object(&self, sha256: &str) -> Result<Vec<u8>, crate::archive::ArchiveError> {
        let Some(allowed) = self.access.get(sha256) else {
            return Err(crate::archive::ArchiveError::AccessDenied(format!(
                "object {sha256} has no committed reference (orphan or unknown; fail-closed)"
            )));
        };
        if !allowed.contains(self.domain) {
            return Err(crate::archive::ArchiveError::AccessDenied(format!(
                "object {sha256} is not visible to domain {:?}",
                self.domain
            )));
        }
        self.archive.read_object_bytes(sha256)
    }

    pub fn read_objects(
        &self,
    ) -> impl Fn(&str) -> Result<Vec<u8>, crate::archive::ArchiveError> + '_ {
        move |sha256: &str| self.read_object(sha256)
    }

    /// The committed manifests of one physical source path.
    pub fn manifests_for(&self, source: &str) -> Vec<&Manifest> {
        self.manifests
            .iter()
            .filter(|manifest| manifest.source.path == source)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Everything one `index` run needs to apply the fallback for one live source.
pub struct FallbackContext {
    rule_version: &'static str,
    archive_root: String,
    domain: String,
    cache_root: Option<PathBuf>,
    fingerprint: String,
    inventory: Option<InventoryFile>,
    archive: Option<Archive>,
    /// The manifest snapshot of this run, loaded on first need and reused by
    /// the inventory derivation and every carrier rebuild (one manifest parse
    /// per source build, never one per carrier).
    snapshot: OnceCell<Result<EvidenceSnapshot, String>>,
    /// Preparation failure (reported as `detail.archive.status = "failed"`).
    error: Option<String>,
}

impl FallbackContext {
    /// Prepare for an index run. `Err` only when no archive source is
    /// configured (the build then runs exactly as before); a configured but
    /// unusable archive returns a degraded context.
    pub fn prepare(config: &RecallConfig) -> Result<FallbackContext, String> {
        let Some(archive_root) = configured_archive_root(config) else {
            return Err("no archive source configured".to_string());
        };
        Ok(FallbackContext::open(
            archive_root,
            config.effective_domain(None),
            config.cache_root.clone().map(PathBuf::from),
        ))
    }

    /// A configured-but-unavailable archive: the live build proceeds, the
    /// detail block reports the failure.
    pub fn unavailable(error: String) -> FallbackContext {
        let mut context = FallbackContext::open(String::new(), String::new(), None);
        context.error = Some(error);
        context
    }

    fn open(archive_root: String, domain: String, cache_root: Option<PathBuf>) -> FallbackContext {
        let mut context = FallbackContext {
            rule_version: super::FALLBACK_RULE_VERSION,
            archive_root,
            domain,
            cache_root,
            fingerprint: String::new(),
            inventory: None,
            archive: None,
            snapshot: OnceCell::new(),
            error: None,
        };
        context.load();
        context
    }

    fn load(&mut self) {
        if self.archive_root.is_empty() {
            return;
        }
        let opened = archive_provider::open(Path::new(&self.archive_root));
        let archive = match opened {
            Ok(archive) => archive,
            Err(failure) => {
                self.error = Some(failure.message);
                return;
            }
        };
        // Change detection is a directory listing: manifest file names embed
        // the unique capture id and are append-only, so no manifest body is
        // parsed unless the evidence set actually changed (rework 3).
        let names = match archive.manifest_names() {
            Ok(names) => names,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        self.fingerprint = manifest_set_fingerprint_from_names(&names);
        self.archive = Some(archive);
        let derived = self.derive_inventory();
        match derived {
            Ok(inventory) => self.inventory = Some(inventory),
            Err(error) => self.error = Some(error),
        }
    }

    /// The run's manifest snapshot: parsed on first need, reused afterwards.
    fn manifest_snapshot(&self) -> Result<&EvidenceSnapshot, String> {
        let loaded = self.snapshot.get_or_init(|| {
            let archive = self
                .archive
                .as_ref()
                .ok_or_else(|| "archive unavailable".to_string())?;
            EvidenceSnapshot::load(archive)
        });
        loaded.as_ref().map_err(|error| error.clone())
    }

    /// A reader over this run's snapshot, for carrier rebuilds and mirrors.
    fn batch(&self) -> Result<EvidenceBatch<'_>, String> {
        let archive = self
            .archive
            .as_ref()
            .ok_or_else(|| "archive unavailable".to_string())?;
        let snapshot = self.manifest_snapshot()?;
        Ok(EvidenceBatch::new(archive, snapshot, &self.domain))
    }

    /// True when the fallback can read evidence right now.
    pub fn available(&self) -> bool {
        self.archive.is_some() && self.inventory.is_some() && self.error.is_none()
    }

    pub fn identity(&self) -> FallbackIdentity {
        FallbackIdentity {
            rule_version: self.rule_version,
            archive_root: self.archive_root.clone(),
            inventory_fingerprint: self.fingerprint.clone(),
        }
    }

    pub fn meta(&self, archive_only_sessions: u64) -> FallbackMeta {
        FallbackMeta {
            rule_version: self.rule_version.to_string(),
            inventory_fingerprint: self.fingerprint.clone(),
            archive_root: self.archive_root.clone(),
            archive_only_sessions,
        }
    }

    /// The `detail.archive` block of one source build.
    pub fn detail(&self, report: &Report, identity_reason: Option<&'static str>) -> Value {
        let mut value = json!({
            "status": if self.available() { "ok" } else { "failed" },
            "rule_version": self.rule_version,
            "inventory_fingerprint": self.fingerprint,
            "archive_root": self.archive_root,
            "archive_only_sessions": report.archive_only_sessions,
            "added": report.added,
            "removed": report.removed,
            "carriers_skipped": report.carriers_skipped,
            "skipped_registered_empty": report.skipped_registered_empty,
        });
        if let Some(reason) = self.error.as_ref().or(report.reason.as_ref()) {
            value["reason"] = json!(reason);
        }
        if let Some(reason) = identity_reason {
            if reason.starts_with("fallback_") {
                value["rebuild_reason"] = json!(reason);
            }
        }
        value
    }

    // -- cache --------------------------------------------------------------

    /// The inventory of the current manifest set: the composed cache file when
    /// it is current, otherwise one derivation pass over this run's snapshot.
    fn derive_inventory(&self) -> Result<InventoryFile, String> {
        let cache_root = self.cache_root.as_deref();
        // The composed inventory file is keyed by the manifest-set fingerprint.
        if let Some(cached) = load_cached_inventory(cache_root, &self.fingerprint) {
            return Ok(cached);
        }
        // Derive: one manifest parse pass groups every physical source; object
        // reads go through the run's snapshot, whose access map is computed on
        // that same pass (rework 3: the per-read handle re-scans all manifests
        // per object, which made a 4 000-carrier cold build quadratic).
        let batch = self.batch()?;
        let mut by_source: BTreeMap<String, Vec<&Manifest>> = BTreeMap::new();
        for manifest in batch.manifests {
            if manifest.source.kind == crate::archive::ATTACHMENT_SOURCE_KIND {
                continue;
            }
            by_source
                .entry(manifest.source.path.clone())
                .or_default()
                .push(manifest);
        }
        let mut groups: Vec<CarrierGroup> = Vec::new();
        for (path, group) in &by_source {
            let selected = select_group_manifests(group);
            let group_fp = group_fingerprint(&selected);
            let provider = selected
                .last()
                .map(|manifest| manifest.provider.clone())
                .unwrap_or_default();
            let kind = selected
                .last()
                .map(|manifest| manifest.source.kind.clone())
                .unwrap_or_default();
            let carrier = selected
                .last()
                .and_then(|manifest| manifest.source.carrier.clone());
            if let Some(cached) =
                cached_group(cache_root, &group_fp, &provider, &kind, carrier.as_deref())
            {
                groups.push(cached);
                continue;
            }
            let extracted = match kind.as_str() {
                "sqlite" => extract_sqlite_group(&batch, path)
                    .map(|(sessions, baseline_digest, capture_seq)| {
                        (sessions, false, baseline_digest, capture_seq)
                    }),
                "jsonl_file" | "whole_file" | "compressed_jsonl_file" => {
                    extract_jsonl_group(&batch, &selected)
                        .map(|(sessions, needs_infer)| (sessions, needs_infer, String::new(), 0u64))
                }
                "dsh_jsonl" => extract_dsh_group(&batch, &selected)
                    .map(|sessions| (sessions, false, String::new(), 0u64)),
                other => Err(format!("unsupported evidence kind {other:?}")),
            };
            match extracted {
                Ok((sessions, needs_infer, baseline_digest, capture_seq)) => {
                    let group = CarrierGroup {
                        path: path.clone(),
                        provider,
                        kind,
                        carrier,
                        fingerprint: group_fp,
                        sessions,
                        needs_infer,
                        baseline_digest,
                        capture_seq,
                    };
                    store_group(cache_root, &group);
                    groups.push(group);
                }
                Err(error) => {
                    // One unreadable carrier never blocks the inventory; the
                    // gap is visible as missing archive sessions until the
                    // next fingerprint change re-derives the group.
                    let _ = error;
                }
            }
        }
        let state = InventoryFile {
            schema: INVENTORY_SCHEMA.to_string(),
            rule_version: super::FALLBACK_RULE_VERSION.to_string(),
            fingerprint: self.fingerprint.clone(),
            generated_at_ms: super::now_ms(),
            groups,
        };
        if let Some(cache_root) = cache_root {
            if let Ok(encoded) = serde_json::to_vec_pretty(&state) {
                let _ = crate::cache_store::write_atomic(
                    cache_root,
                    &["archive-inventory", "current.json"],
                    &encoded,
                );
            }
            prune_session_cache(cache_root, &state);
        }
        Ok(state)
    }

    /// Groups of one provider (path order, deterministic).
    fn groups_for(&self, provider: &str) -> Vec<CarrierGroup> {
        self.inventory
            .as_ref()
            .map(|inventory| {
                inventory
                    .groups
                    .iter()
                    .filter(|group| group.provider == provider)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The composed inventory file, when it describes this exact evidence set.
fn load_cached_inventory(cache_root: Option<&Path>, fingerprint: &str) -> Option<InventoryFile> {
    let cache_root = cache_root?;
    let path = crate::cache_store::join_checked(
        cache_root,
        &["archive-inventory", "current.json"],
    )
    .ok()?;
    let state: InventoryFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if state.fingerprint == fingerprint
        && state.rule_version == super::FALLBACK_RULE_VERSION
        && state.schema == INVENTORY_SCHEMA
    {
        Some(state)
    } else {
        None
    }
}

fn cached_group(
    cache_root: Option<&Path>,
    fingerprint: &str,
    provider: &str,
    kind: &str,
    carrier: Option<&str>,
) -> Option<CarrierGroup> {
    let cache_root = cache_root?;
    let name = format!("{fingerprint}.json");
    let path =
        crate::cache_store::join_checked(cache_root, &["archive-inventory", "sessions", &name])
            .ok()?;
    let cached: CarrierGroup = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if cached.fingerprint == fingerprint
        && cached.provider == provider
        && cached.kind == kind
        && cached.carrier.as_deref() == carrier
    {
        Some(cached)
    } else {
        None
    }
}

fn store_group(cache_root: Option<&Path>, group: &CarrierGroup) {
    let Some(cache_root) = cache_root else {
        return;
    };
    let Ok(encoded) = serde_json::to_vec(group) else {
        return;
    };
    let name = format!("{}.json", group.fingerprint);
    let _ = crate::cache_store::write_atomic(
        cache_root,
        &["archive-inventory", "sessions", &name],
        &encoded,
    );
}

fn prune_session_cache(cache_root: &Path, state: &InventoryFile) {
    let live: BTreeSet<String> = state
        .groups
        .iter()
        .map(|group| format!("{}.json", group.fingerprint))
        .collect();
    let Ok(entries) = std::fs::read_dir(cache_root.join("archive-inventory").join("sessions"))
    else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") && !live.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Session ids of one archived JSONL carrier: the ids the live builder would
/// assign (Claude: every record's `sessionId`; Codex: the first
/// `session_meta.payload.id`).
fn extract_jsonl_group(
    batch: &EvidenceBatch<'_>,
    selected: &[&Manifest],
) -> Result<(BTreeSet<String>, bool), String> {
    // Only Claude records may fall back to the rel-path inference; every
    // Codex response_item belongs to its session_meta marker session.
    let provider = selected
        .last()
        .map(|manifest| manifest.provider.clone())
        .unwrap_or_default();
    let bytes = rebuild_jsonl_bytes(batch, selected)?;
    let mut sessions = BTreeSet::new();
    let mut needs_infer = false;
    let mut first = true;
    let mut cursor = Cursor::new(bytes.as_slice());
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        let line = crate::recall::port::reader::read_line_bounded(
            &mut cursor,
            &mut buffer,
            super::build::INDEX_LINE_LIMIT_BYTES,
            false,
        )
        .map_err(|error| error.to_string())?;
        match line {
            crate::recall::port::reader::BoundedLine::Eof => break,
            crate::recall::port::reader::BoundedLine::Line { too_long, .. } => {
                if too_long {
                    continue;
                }
            }
        }
        while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
            buffer.pop();
        }
        if buffer.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&buffer) else {
            continue;
        };
        if first {
            first = false;
            // Codex: the marker rule of the live builder.
            if let Some(marker) = codex_marker_from_value(&value) {
                if let Some(session_id) = marker.session_id {
                    sessions.insert(session_id);
                    continue;
                }
            }
        }
        // Claude: the per-record rule of the live builder. A record without
        // its own sessionId indexes under the rel-path inference, so the
        // inferred id joins the session set at apply time.
        match crate::recall::providers::claude_code::record_meta(&value).session_id {
            Some(session_id) => {
                sessions.insert(session_id);
            }
            None if provider == "claude" => needs_infer = true,
            None => {}
        }
    }
    Ok((sessions, needs_infer))
}

/// Session ids of one archived DSH carrier: the session header's `id`.
///
/// A DSH carrier is a decoded logical JSONL stream whose first complete line is
/// the session header. A carrier without a readable header contributes no
/// session (never a guess from the directory name).
fn extract_dsh_group(
    batch: &EvidenceBatch<'_>,
    selected: &[&Manifest],
) -> Result<BTreeSet<String>, String> {
    let bytes = rebuild_jsonl_bytes(batch, selected)?;
    let mut sessions = BTreeSet::new();
    let mut cursor = Cursor::new(bytes.as_slice());
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        let line = crate::recall::port::reader::read_line_bounded(
            &mut cursor,
            &mut buffer,
            super::build::INDEX_LINE_LIMIT_BYTES,
            false,
        )
        .map_err(|error| error.to_string())?;
        match line {
            crate::recall::port::reader::BoundedLine::Eof => break,
            crate::recall::port::reader::BoundedLine::Line { too_long, .. } => {
                if too_long {
                    continue;
                }
            }
        }
        while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
            buffer.pop();
        }
        if buffer.is_empty() {
            continue;
        }
        if let Some(header) = dsh::parse_header(&buffer) {
            sessions.insert(header.session_id);
        }
        // Only the header identifies the session; later lines need not be read.
        break;
    }
    Ok(sessions)
}

/// Session ids of one archived SQLite source: the `session_v2` rows at the
/// latest committed observation point. Only session-table evidence is
/// decompressed (the message payload stays unread until a session is wanted).
/// The session tables whose ids make up the archived inventory: V2 (the
/// generation the live index covers) plus V1 (sessions that were never
/// migrated and are invisible to the live builder).
const INVENTORY_SESSION_TABLES: [&str; 2] = ["session_v2", "session"];

/// Session ids of one archived SQLite source: every `session_v2` / `session`
/// row the evidence holds - the baseline rows **plus** every delta-touched id
/// under delete-keeping semantics (a committed delete stamps the deletion but
/// keeps the row content recoverable, so a session the user deleted in
/// OpenCode stays in the archived set). Only session-table evidence is
/// decompressed here; message payloads stay unread until a session is wanted.
fn extract_sqlite_group(
    batch: &EvidenceBatch<'_>,
    path: &str,
) -> Result<(BTreeSet<String>, String, u64), String> {
    let baselines = crate::evidence::sqlite_baselines_in(batch.manifests, path);
    let baseline = baselines
        .last()
        .ok_or_else(|| "no SQLite baseline committed".to_string())?;
    let mut sessions = BTreeSet::new();
    // Session tables are small: one chunk scan, ids only (streamed per chunk,
    // no row materialization).
    let fetch = batch.read_objects();
    for chunk in &baseline.chunks {
        if !INVENTORY_SESSION_TABLES.contains(&chunk.table.as_str()) {
            continue;
        }
        for line in crate::evidence::read_chunk_rows_fetch(&fetch, chunk)
            .map_err(|error| error.to_string())?
        {
            if let Some(id) = line.row.get("id").and_then(Value::as_str) {
                sessions.insert(id.to_string());
            }
        }
    }
    let mut capture_seq = baseline.capture_seq;
    for meta in crate::evidence::sqlite_delta_batch_metas_in(batch.manifests, path) {
        if meta.baseline_generation != baseline.baseline_generation
            || meta.capture_seq <= baseline.capture_seq
        {
            continue;
        }
        for line in crate::evidence::read_delta_lines_fetch(&fetch, &meta)
            .map_err(|error| error.to_string())?
        {
            if !INVENTORY_SESSION_TABLES.contains(&line.table.as_str()) {
                continue;
            }
            if let Some(after) = line.after.as_ref() {
                if let Some(id) = after.get("id").and_then(Value::as_str) {
                    sessions.insert(id.to_string());
                    capture_seq = capture_seq.max(meta.capture_seq);
                    continue;
                }
            }
            sessions.insert(row_id_from_key(&line.key));
            capture_seq = capture_seq.max(meta.capture_seq);
        }
    }
    Ok((sessions, baseline.baseline_digest.clone(), capture_seq))
}

// ---------------------------------------------------------------------------
// JSONL carrier rebuild
// ---------------------------------------------------------------------------

/// Splice one archived JSONL carrier back into its logical bytes: the selected
/// manifests' objects ordered by `source_offset`, decoded through their content
/// addresses (a `.jsonl.zst` carrier serves its `decoded` object).
pub(crate) fn rebuild_jsonl_bytes(
    batch: &EvidenceBatch<'_>,
    selected: &[&Manifest],
) -> Result<Vec<u8>, String> {
    let compressed = selected
        .last()
        .map(|manifest| manifest.source.kind == "compressed_jsonl_file")
        .unwrap_or(false);
    if compressed {
        let manifest = selected.last().expect("non-empty");
        let decoded = manifest
            .objects
            .iter()
            .find(|object| object.role == "decoded")
            .ok_or_else(|| "compressed carrier has no decoded object".to_string())?;
        return batch
            .read_object(&decoded.sha256)
            .map_err(|error| error.to_string());
    }
    let mut ordered: Vec<&Manifest> = selected.to_vec();
    ordered.sort_by_key(|manifest| manifest.source.range_start);
    let mut out: Vec<u8> = Vec::new();
    let mut expected: u64 = 0;
    for manifest in ordered {
        let range_start = manifest.source.range_start;
        if range_start != expected {
            return Err(format!(
                "committed byte ranges are not contiguous at {} (expected {expected})",
                manifest.source.path
            ));
        }
        let object = manifest
            .objects
            .first()
            .ok_or_else(|| "capture manifest has no object".to_string())?;
        let bytes = batch
            .read_object(&object.sha256)
            .map_err(|error| error.to_string())?;
        expected = expected.saturating_add(bytes.len() as u64);
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Build-side application
// ---------------------------------------------------------------------------

/// One source's fallback application report (also the `detail.archive` input).
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub archive_only_sessions: u64,
    pub added: u64,
    pub removed: u64,
    pub carriers_skipped: u64,
    /// Archive-only sessions skipped this run because their registration says
    /// the current rule version projected zero rows for them (§D).
    pub skipped_registered_empty: u64,
    pub reason: Option<String>,
}

impl Report {
    pub fn new() -> Report {
        Report::default()
    }

    pub fn failed(reason: String) -> Report {
        Report {
            reason: Some(reason),
            ..Report::default()
        }
    }
}

/// What this run still has to do for one archive-only session (§D).
enum ArchiveWork {
    /// Project it from the evidence now: never registered, registered by an
    /// older rule version, or registered with rows that are gone.
    Pending,
    /// Registered by the current rule version with a **zero-row** projection:
    /// re-reading the carrier cannot produce rows, so the session is skipped
    /// and counted instead of being re-decompressed every run.
    SkippedEmpty,
    /// Registered by the current rule version and its rows are still present.
    Present,
}

/// Carrier-group fingerprint of one session: every group of this source that
/// holds the session, sorted, deduplicated, joined and hashed.
///
/// A zero-row registration is only valid for the exact evidence it read, so
/// the registry binds it to this value: a new capture of the session's carrier
/// changes its group fingerprint and forces the session through the projector
/// again. A session handled through one group hashes that one fingerprint
/// (`hex_sha256(fingerprint)`), which keeps the value stable across runs.
fn carrier_fingerprint_for(
    fingerprints: &BTreeMap<String, BTreeSet<String>>,
    session: &str,
) -> String {
    match fingerprints.get(session) {
        Some(groups) => {
            let joined = groups.iter().cloned().collect::<Vec<_>>().join("\n");
            crate::archive::hex_sha256(joined.as_bytes())
        }
        None => String::new(),
    }
}

/// Decide one session's work from the registration alone.
///
/// Rebuild when: not registered, `rule_version` is missing/old, the recorded
/// carrier fingerprint is missing or differs from the current one, or
/// `projected_units > 0` while the index no longer holds its archive rows.
/// Only a registration that matches **all three** facts and projected zero
/// rows is skipped. A full rebuild clears the registry (`Writer::reset`), so
/// those sessions are processed once again before being re-registered.
fn archive_work(
    writer: &Writer<'_>,
    session: &str,
    current_fingerprint: &str,
) -> Result<ArchiveWork, String> {
    let state = writer
        .archive_session_state(session)
        .map_err(|error| error.to_string())?;
    let Some(registration) = state else {
        return Ok(ArchiveWork::Pending);
    };
    if registration.rule_version.as_deref() != Some(super::FALLBACK_RULE_VERSION) {
        return Ok(ArchiveWork::Pending);
    }
    // A registration without, or with a different, carrier fingerprint was
    // written against other evidence: re-read it (the carrier may now hold
    // content the empty projection never saw).
    match registration.carrier_fingerprint.as_deref() {
        Some(recorded) if !recorded.is_empty() && recorded == current_fingerprint => {}
        _ => return Ok(ArchiveWork::Pending),
    }
    match registration.projected_units {
        Some(0) => Ok(ArchiveWork::SkippedEmpty),
        Some(_) if writer.archive_rows_present(session) => Ok(ArchiveWork::Present),
        // Rows vanished from the index: rebuild from the frozen evidence.
        Some(_) => Ok(ArchiveWork::Pending),
        // A registration written before the columns existed.
        None => Ok(ArchiveWork::Pending),
    }
}

/// Split the not-live sessions of one carrier into work and skips. Skips are
/// collected by session id, so a session held by several carriers is counted
/// once by the caller.
fn partition_archive_work(
    writer: &Writer<'_>,
    sessions: &BTreeSet<String>,
    fingerprints: &BTreeMap<String, BTreeSet<String>>,
    skipped: &mut BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let mut pending = Vec::new();
    for session in sessions {
        let current = carrier_fingerprint_for(fingerprints, session);
        match archive_work(writer, session, &current)? {
            ArchiveWork::Pending => pending.push(session.clone()),
            ArchiveWork::SkippedEmpty => {
                skipped.insert(session.clone());
            }
            ArchiveWork::Present => {}
        }
    }
    Ok(pending)
}

/// Index the carrier-group fingerprints of every session of this source.
fn session_fingerprints<'a>(
    entries: impl IntoIterator<Item = (&'a CarrierGroup, &'a BTreeSet<String>)>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut by_session: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (group, sessions) in entries {
        for session in sessions {
            by_session
                .entry(session.clone())
                .or_default()
                .insert(group.fingerprint.clone());
        }
    }
    by_session
}

/// rel path of an archived carrier inside the live source root (Codex
/// compressed carriers map to their logical `.jsonl` name — the same rel the
/// live builder would use once Codex decompresses them again).
fn carrier_rel(live_root: &str, carrier_path: &str, kind: &str) -> Option<String> {
    let path = Path::new(carrier_path);
    let logical = if kind == "compressed_jsonl_file" {
        crate::codex_rollout_file::logical_path(path)
    } else {
        path.to_path_buf()
    };
    rel_from_root(live_root, &logical)
}

/// Rebuild one carrier group's bytes through the run's manifest snapshot.
fn rebuild_group_bytes(batch: &EvidenceBatch<'_>, group: &CarrierGroup) -> Result<Vec<u8>, String> {
    let group_manifests = batch.manifests_for(&group.path);
    let selected = select_group_manifests(&group_manifests);
    rebuild_jsonl_bytes(batch, &selected)
}

/// Apply the fallback for a Claude Code / Codex source after its live scan.
pub(super) fn apply_jsonl(
    context: &FallbackContext,
    source: &SourceConfig,
    writer: &mut Writer<'_>,
) -> Result<Report, String> {
    let mut report = Report::new();
    if !context.available() {
        report.reason = context.error.clone();
        return Ok(report);
    }
    let provider = match source.kind {
        SourceKind::ClaudeCode => "claude",
        SourceKind::Codex => "codex",
        _ => return Ok(report),
    };
    let live_sessions = writer
        .live_sessions()
        .map_err(|error| error.to_string())?;
    let registered = writer
        .archive_sessions()
        .map_err(|error| error.to_string())?;

    // Map archived carrier groups onto this source's rel paths. The full
    // session set of every carrier is kept beside the missing subset: the
    // registry fingerprint is derived from the carriers that hold a session.
    let mut carriers: Vec<(String, CarrierGroup, BTreeSet<String>, BTreeSet<String>)> = Vec::new();
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for group in context.groups_for(provider) {
        let Some(rel) = carrier_rel(&source.root, &group.path, &group.kind) else {
            report.carriers_skipped += 1;
            continue;
        };
        // The live builder never indexes a Claude journal; the fallback must
        // not resurrect one under its file-name "session id".
        if provider == "claude" && rel.split(['/', '\\']).last() == Some("journal.jsonl") {
            report.carriers_skipped += 1;
            continue;
        }
        let mut sessions = group.sessions.clone();
        if group.needs_infer {
            // Records without their own sessionId index under the rel-path
            // inference (the live Claude rule).
            if let Some(inferred) = crate::recall::providers::claude_code::infer_session_id(&rel) {
                sessions.insert(inferred);
            }
        }
        if provider == "codex" && sessions.is_empty() {
            // Live rule: the session_meta marker wins; the file-name inference
            // applies only when the carrier has no marker at all.
            if let Some(inferred) =
                crate::providers::codex::infer_session_id_from_filename(Path::new(&rel))
            {
                sessions.insert(inferred);
            }
        }
        let missing: BTreeSet<String> = sessions
            .iter()
            .filter(|session| !live_sessions.contains(*session))
            .cloned()
            .collect();
        for session in &missing {
            wanted.insert(session.clone());
        }
        carriers.push((rel, group, missing, sessions));
    }

    let fingerprints = session_fingerprints(
        carriers
            .iter()
            .map(|(_, group, _, sessions)| (group, sessions)),
    );

    // Drop registry entries whose session is live again (or gone entirely):
    // origin-aware removal never touches live rows of the same session.
    for session in registered.difference(&wanted) {
        writer
            .remove_archive_session(session)
            .map_err(|error| error.to_string())?;
        writer
            .forget_archive_session(session)
            .map_err(|error| error.to_string())?;
        report.removed += 1;
    }

    // Decide every carrier against the **same** registry snapshot before
    // anything is registered: a session whose id is held by several carriers
    // must be projected from all of them in this run (a zero-row carrier that
    // sorts first must not hide the real content of a later one).
    let mut pending_by_carrier: Vec<Vec<String>> = Vec::with_capacity(carriers.len());
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    for (_, _, missing, _) in &carriers {
        pending_by_carrier.push(partition_archive_work(
            writer,
            missing,
            &fingerprints,
            &mut skipped,
        )?);
    }
    report.skipped_registered_empty += skipped.len() as u64;

    // Rebuild carriers that still have wanted sessions without rows. A
    // wanted session's evidence is frozen, so known-present rows are reused
    // and a session registered with an empty projection is not re-read at all.
    // The manifest snapshot is loaded on the first rebuild of this run and
    // shared by all of them (one manifest parse, not one per carrier).
    let mut batch: Option<EvidenceBatch<'_>> = None;
    let mut projected: BTreeSet<String> = BTreeSet::new();
    for ((rel, group, _, _), pending) in carriers.iter().zip(pending_by_carrier) {
        if pending.is_empty() {
            continue;
        }
        projected.extend(pending.iter().cloned());
        if batch.is_none() {
            batch = Some(context.batch()?);
        }
        let batch = batch.as_ref().expect("snapshot loaded above");
        let bytes = {
            let _rebuild = crate::cost::phase("archive_carrier_rebuild");
            rebuild_group_bytes(batch, group)?
        };
        let marker = if provider == "codex" {
            codex_marker_from_reader(&mut Cursor::new(bytes.as_slice()))
        } else {
            None
        };
        // A carrier may hold several session ids; only the wanted ones get
        // archive rows — a carrier sibling the live source still serves must
        // never be duplicated here (verifier B3).
        let whitelist: BTreeSet<String> = pending.iter().cloned().collect();
        {
            let _projection = crate::cost::phase("archive_projection");
            scan_jsonl_reader(
                source,
                rel,
                Cursor::new(bytes.as_slice()),
                0,
                0,
                marker.as_ref(),
                writer,
                ORIGIN_ARCHIVE,
                Some(&whitelist),
            )
            .map_err(|error| error.message())?;
        }
    }

    // Register each session once, with the units all of its carriers produced
    // (`added` counts sessions, not carriers).
    for session in &projected {
        let units = writer
            .archive_unit_count(session)
            .map_err(|error| error.to_string())?;
        let fingerprint = carrier_fingerprint_for(&fingerprints, session);
        writer
            .remember_archive_session(session, units, &fingerprint)
            .map_err(|error| error.to_string())?;
        report.added += 1;
    }

    report.archive_only_sessions = writer
        .archive_sessions()
        .map(|sessions| sessions.len() as u64)
        .unwrap_or(0);
    Ok(report)
}

/// Apply the fallback for an OpenCode source after its live scan.
pub(super) fn apply_opencode(
    context: &FallbackContext,
    source: &SourceConfig,
    tx: &rusqlite::Transaction<'_>,
    writer: &mut Writer<'_>,
) -> Result<Report, String> {
    let mut report = Report::new();
    if !context.available() {
        report.reason = context.error.clone();
        return Ok(report);
    }
    let live_sessions = writer
        .opencode_live_sessions()
        .map_err(|error| error.to_string())?;
    let registered = writer
        .archive_sessions()
        .map_err(|error| error.to_string())?;

    // The archived SQLite evidence this live database was captured from: the
    // group whose recorded path is this database, or the only one there is.
    let db_path = match source.db.as_deref() {
        Some(value) => PathBuf::from(value),
        None => Path::new(&source.root).join("opencode.db"),
    };
    let groups = context.groups_for("opencode");
    let Some(group) = select_sqlite_group(&groups, &db_path) else {
        report.carriers_skipped += groups.len() as u64;
        report.reason = Some("no matching archived sqlite source".to_string());
        return Ok(report);
    };
    let wanted: BTreeSet<String> = group
        .sessions
        .iter()
        .filter(|session| !live_sessions.contains(*session))
        .cloned()
        .collect();

    for session in registered.difference(&wanted) {
        writer
            .remove_archive_session(session)
            .map_err(|error| error.to_string())?;
        writer
            .forget_archive_session(session)
            .map_err(|error| error.to_string())?;
        report.removed += 1;
    }

    let fingerprints = session_fingerprints(std::iter::once((&group, &group.sessions)));
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    let pending = partition_archive_work(writer, &wanted, &fingerprints, &mut skipped)?;
    report.skipped_registered_empty += skipped.len() as u64;
    if !pending.is_empty() {
        let cache_root = context
            .cache_root
            .clone()
            .ok_or_else(|| "cache_root unavailable for the row mirror".to_string())?;
        let mirror = {
            let _rebuild = crate::cost::phase("archive_carrier_rebuild");
            ensure_row_mirror(context, &group, &cache_root, &wanted)?
        };
        let index = MirrorReader::open(&mirror).map_err(|error| error.to_string())?;
        for session in &pending {
            if !index.has_session(session) {
                // Unknown to the evidence (stale inventory): forget the entry
                // so the next inventory refresh decides again.
                writer
                    .forget_archive_session(session)
                    .map_err(|error| error.to_string())?;
                continue;
            }
            // A session present in both generations is indexed from V2 only
            // (V2 is what the live index covers; the V1 rows of a migrated
            // session are the same conversation).
            let added = {
                let _projection = crate::cost::phase("archive_projection");
                if index.v2_session_exists(session) {
                    index_session_rows(source, &index, session, tx, writer)?
                } else {
                    index_v1_session_rows(source, &mirror, session, tx, writer)?
                }
            };
            let units = writer
                .archive_unit_count(session)
                .map_err(|error| error.to_string())?;
            let fingerprint = carrier_fingerprint_for(&fingerprints, session);
            writer
                .remember_archive_session(session, units, &fingerprint)
                .map_err(|error| error.to_string())?;
            report.added += added;
        }
    }

    report.archive_only_sessions = writer
        .archive_sessions()
        .map(|sessions| sessions.len() as u64)
        .unwrap_or(0);
    Ok(report)
}

/// Apply the fallback for a DSH source after its live scan.
///
/// The archived carrier is rebuilt from the committed logical ranges, decoded
/// as one logical stream and projected with the **live** DSH rules
/// (`dsh::project_event` via `build::dsh_record_units`), so an archived row and a
/// live row of the same event are identical apart from `origin` and the
/// archive-fallback locator marker.
pub(super) fn apply_dsh(
    context: &FallbackContext,
    _source: &SourceConfig,
    writer: &mut Writer<'_>,
) -> Result<Report, String> {
    let mut report = Report::new();
    if !context.available() {
        report.reason = context.error.clone();
        return Ok(report);
    }
    let live_sessions = writer
        .live_sessions()
        .map_err(|error| error.to_string())?;
    let registered = writer
        .archive_sessions()
        .map_err(|error| error.to_string())?;

    let mut carriers: Vec<(String, CarrierGroup, BTreeSet<String>)> = Vec::new();
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for group in context.groups_for("dsh") {
        // The generation carrier is a capture fact (`source.carrier`); a group
        // without one is not readable evidence and is never guessed from the
        // directory name.
        let Some(rel) = group.carrier.clone() else {
            report.carriers_skipped += 1;
            continue;
        };
        if group.sessions.is_empty() {
            report.carriers_skipped += 1;
            continue;
        }
        let missing: BTreeSet<String> = group
            .sessions
            .iter()
            .filter(|session| !live_sessions.contains(*session))
            .cloned()
            .collect();
        for session in &missing {
            wanted.insert(session.clone());
        }
        carriers.push((rel, group, missing));
    }

    for session in registered.difference(&wanted) {
        writer
            .remove_archive_session(session)
            .map_err(|error| error.to_string())?;
        writer
            .forget_archive_session(session)
            .map_err(|error| error.to_string())?;
        report.removed += 1;
    }

    let fingerprints = session_fingerprints(
        carriers
            .iter()
            .map(|(_, group, _)| (group, &group.sessions)),
    );
    // One manifest snapshot for every carrier rebuild of this run.
    let mut batch: Option<EvidenceBatch<'_>> = None;
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    for (rel, group, missing) in &carriers {
        let pending = partition_archive_work(writer, missing, &fingerprints, &mut skipped)?;
        if pending.is_empty() {
            continue;
        }
        if batch.is_none() {
            batch = Some(context.batch()?);
        }
        let bytes = {
            let _rebuild = crate::cost::phase("archive_carrier_rebuild");
            rebuild_group_bytes(batch.as_ref().expect("snapshot loaded above"), group)?
        };
        let log = dsh::log_from_logical(rel, bytes);
        let dir_name = Path::new(rel)
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(meta) = dsh::scan_meta(&log, &dir_name) else {
            report.carriers_skipped += 1;
            continue;
        };
        let whitelist: BTreeSet<String> = pending.iter().cloned().collect();
        if !whitelist.contains(&meta.session_id) {
            report.carriers_skipped += 1;
            continue;
        }
        let mut added = 0u64;
        let projection = crate::cost::phase("archive_projection");
        for line in log.lines() {
            let Ok(value) = serde_json::from_slice::<Value>(line.bytes) else {
                continue;
            };
            let row_sha256 = sha256_hex(line.bytes);
            for mut unit in crate::recall::fulltext::build::dsh_record_units(
                rel,
                line.index,
                &value,
                &row_sha256,
                &meta,
            ) {
                if !whitelist.contains(&unit.session_id) {
                    continue;
                }
                unit.origin = ORIGIN_ARCHIVE;
                unit.locator.archive_fallback =
                    Some(crate::recall::refs::ArchiveLocator::record(&row_sha256));
                writer
                    .insert(&unit)
                    .map_err(|error| format!("{rel}: {error}"))?;
            }
        }
        drop(projection);
        for session in &pending {
            let units = writer
                .archive_unit_count(session)
                .map_err(|error| error.to_string())?;
            let fingerprint = carrier_fingerprint_for(&fingerprints, session);
            writer
                .remember_archive_session(session, units, &fingerprint)
                .map_err(|error| error.to_string())?;
            added += 1;
        }
        report.added += added;
    }
    report.skipped_registered_empty += skipped.len() as u64;

    report.archive_only_sessions = writer
        .archive_sessions()
        .map(|sessions| sessions.len() as u64)
        .unwrap_or(0);
    Ok(report)
}

fn select_sqlite_group(groups: &[CarrierGroup], db_path: &Path) -> Option<CarrierGroup> {
    let mut normalized: BTreeMap<String, &CarrierGroup> = BTreeMap::new();
    for group in groups {
        let key = group.path.replace('/', "\\").to_ascii_lowercase();
        normalized.insert(key, group);
    }
    let candidates = [db_path.to_string_lossy().replace('/', "\\").to_ascii_lowercase()];
    for candidate in candidates {
        if let Some(group) = normalized.get(&candidate) {
            return Some((*group).clone());
        }
    }
    if groups.len() == 1 {
        return Some(groups[0].clone());
    }
    None
}

// ---------------------------------------------------------------------------
// OpenCode row mirror (baseline-digest keyed materialization)
// ---------------------------------------------------------------------------


/// Read-only accessor over the row mirror of one baseline generation.
pub struct MirrorReader {
    conn: rusqlite::Connection,
}

impl MirrorReader {
    pub fn open(path: &Path) -> Result<MirrorReader, String> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|error| error.to_string())?;
        Ok(MirrorReader { conn })
    }

    /// True when either generation holds the session (V2 `session_v2` or V1
    /// `session`, including rows a committed delete kept).
    pub fn has_session(&self, session_id: &str) -> bool {
        for table in ["session_v2", "session"] {
            let present: i64 = self
                .conn
                .query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM {table} WHERE id = ?1)"
                    ),
                    [session_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if present != 0 {
                return true;
            }
        }
        false
    }

    /// True when the V2 evidence holds the session (same-id rule: a migrated
    /// session is always indexed from V2, never from its V1 copy).
    pub fn v2_session_exists(&self, session_id: &str) -> bool {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session_v2 WHERE id = ?1)",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .unwrap_or(false)
    }

    /// One V1 session row: `None` when the V1 evidence has no such session.
    pub fn v1_session(&self, session_id: &str) -> Result<Option<MirrorSessionInfo>, String> {
        let info: Option<MirrorSessionInfo> = self
            .conn
            .query_row(
                "SELECT title, parent_id, deleted_at_ms FROM session WHERE id = ?1",
                [session_id],
                |row| {
                    Ok(MirrorSessionInfo {
                        title: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        parent_id: row.get(1)?,
                        deleted_at_ms: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        Ok(info)
    }

    /// One session's whitelisted V2 rows in V2 order (`seq, id`).
    pub fn session(&self, session_id: &str) -> Result<MirrorSession, String> {
        let (title, parent_id, deleted_at_ms): (Option<String>, Option<String>, i64) = self
            .conn
            .query_row(
                "SELECT title, parent_id, deleted_at_ms FROM session_v2 WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| error.to_string())?;
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, session_id, seq, type, time_created, data FROM session_message \
                 WHERE session_id = ?1 ORDER BY seq, id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, session_id, seq, message_type, time_created, data) =
                row.map_err(|error| error.to_string())?;
            let row_sha256 = sha256_hex(data.as_bytes());
            out.push(MirrorRow {
                id,
                session_id,
                seq,
                time_created,
                message_type,
                data,
                row_sha256,
            });
        }
        Ok(MirrorSession {
            title: title.unwrap_or_default(),
            parent_id,
            deleted_at_ms,
            rows: out,
        })
    }
}

/// Session-row facts shared by both generations.
pub struct MirrorSessionInfo {
    pub title: String,
    pub parent_id: Option<String>,
    /// `0` when the session still exists in the evidence state; the deleting
    /// delta's observation time when the user deleted it (content kept).
    pub deleted_at_ms: i64,
}

/// One wanted session's rows as the V2 index builder expects them.
pub struct MirrorSession {
    pub title: String,
    pub parent_id: Option<String>,
    pub deleted_at_ms: i64,
    pub rows: Vec<MirrorRow>,
}

#[derive(Debug, Clone)]
pub struct MirrorRow {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub time_created: i64,
    pub message_type: String,
    /// The `data` column verbatim (the JSON text SQLite holds).
    pub data: String,
    pub row_sha256: String,
}

/// Clean stale `.building.<pid>` staging files of the same baseline digest
/// (crash leftovers from earlier runs; verifier NB-5).
fn clean_stale_staging(dir: &Path, digest: &str) {
    let prefix = format!("{digest}.building.");
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Ensure the row mirror covers exactly the **wanted sessions** (rework 3:
/// the mirror is on-demand). Lifecycle over one baseline generation:
///
/// * fresh build: baseline rows of the wanted sessions, then every delta of
///   the generation filtered to them (delete-keeping, so a deleted session
///   keeps its pre-deletion content and `deleted_at_ms`);
/// * growth: newly wanted sessions get their baseline rows extracted and the
///   full delta range applied - delta lines are content-complete (`before` /
///   `after` rows), so re-applying the already-applied range for known
///   sessions is idempotent;
/// * shrink: rows of sessions that left the wanted set are deleted;
/// * advance: pending deltas of the generation, filtered to the wanted set.
///
/// All statements run in one transaction on the mirror; returns the path.
fn ensure_row_mirror(
    context: &FallbackContext,
    group: &CarrierGroup,
    cache_root: &Path,
    wanted: &BTreeSet<String>,
) -> Result<PathBuf, String> {
    let dir = cache_root.join("archive-inventory").join("rows");
    let batch = context.batch()?;
    let baselines = crate::evidence::sqlite_baselines_in(batch.manifests, &group.path);
    // Prefer the generation the inventory was derived from; if evidence moved
    // on in between (a capture committed a newer baseline after `prepare`),
    // serve from the newest committed generation - the next inventory refresh
    // re-keys the mirror.
    let baseline = baselines
        .iter()
        .find(|baseline| baseline.baseline_digest == group.baseline_digest)
        .or_else(|| baselines.last())
        .ok_or_else(|| "mirror baseline is no longer committed".to_string())?;
    let digest = baseline.baseline_digest.clone();
    std::fs::create_dir_all(&dir).map_err(|error| format!("mirror dir: {error}"))?;
    clean_stale_staging(&dir, &digest);
    let mirror = dir.join(format!("{digest}.sqlite"));
    let max_seq = crate::evidence::sqlite_delta_batch_metas_in(batch.manifests, &group.path)
        .iter()
        .filter(|meta| meta.baseline_generation == baseline.baseline_generation)
        .map(|meta| meta.capture_seq)
        .max()
        .unwrap_or(baseline.capture_seq);

    let state = MirrorState::load(&mirror).filter(|stored| {
        stored.schema == MIRROR_SCHEMA_VERSION
            && stored.source_path == group.path
            && stored.baseline_digest == digest
    });
    let known: BTreeSet<String> = state
        .as_ref()
        .map(|stored| stored.sessions.iter().cloned().collect())
        .unwrap_or_default();
    let new_sessions: BTreeSet<String> = wanted.difference(&known).cloned().collect();
    let gone: BTreeSet<String> = known.difference(wanted).cloned().collect();
    let pending_deltas = state
        .as_ref()
        .map(|state| state.last_capture_seq < max_seq)
        .unwrap_or(false);
    if let Some(stored) = &state {
        if new_sessions.is_empty() && gone.is_empty() && !pending_deltas {
            let _ = stored;
            return Ok(mirror);
        }
    } else if wanted.is_empty() {
        return Ok(mirror);
    }

    let run = |mirror: &Path, fresh: bool| -> Result<(), String> {
        let mut conn = rusqlite::Connection::open(mirror)
            .map_err(|error| format!("mirror open: {error}"))?;
        if fresh {
            // Best effort: pragma failures only slow the rebuild down.
            let _ = tune_rebuildable_cache(&conn);
            conn.execute_batch(MIRROR_SCHEMA)
                .map_err(|error| error.to_string())?;
        }
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        let extract: BTreeSet<String> = if fresh {
            wanted.clone()
        } else {
            new_sessions.clone()
        };
        if !extract.is_empty() {
            insert_baseline_session_rows(&batch, baseline, &tx, &extract)?;
        }
        let after = if fresh || !new_sessions.is_empty() {
            baseline.capture_seq
        } else {
            state
                .as_ref()
                .map(|state| state.last_capture_seq)
                .unwrap_or(baseline.capture_seq)
        };
        let capture_seq = apply_session_deltas(
            &batch,
            &group.path,
            &tx,
            wanted,
            baseline.baseline_generation,
            after,
            max_seq,
        )?;
        for session in &gone {
            for table in ["session_v2", "session"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE id = ?1"),
                    rusqlite::params![session],
                )
                .map_err(|error| error.to_string())?;
            }
            for table in ["session_message", "message", "part"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE session_id = ?1"),
                    rusqlite::params![session],
                )
                .map_err(|error| error.to_string())?;
            }
        }
        let sessions: Vec<String> = wanted.iter().cloned().collect();
        MirrorState {
            schema: MIRROR_SCHEMA_VERSION.to_string(),
            source_path: group.path.clone(),
            baseline_digest: digest.clone(),
            last_capture_seq: capture_seq.max(max_seq),
            sessions,
        }
        .store(&tx)?;
        tx.commit().map_err(|error| error.to_string())
    };

    if state.is_some() && mirror.exists() {
        run(&mirror, false)?;
        return Ok(mirror);
    }
    // Fresh mirror: staging file, then an atomic rename into place.
    let staging = dir.join(format!("{}.building.{}", digest, std::process::id()));
    let _ = std::fs::remove_file(&staging);
    run(&staging, true)?;
    std::fs::rename(&staging, &mirror).map_err(|error| format!("mirror rename: {error}"))?;
    Ok(mirror)
}

/// Mirror layout version. A stored mirror with a different version is rebuilt
/// (the mirror is a deletable cache; there is no in-place migration).
pub const MIRROR_SCHEMA_VERSION: &str = "agent-memory/opencode-row-mirror@3";

/// The mirror reproduces the whitelisted OpenCode row state - V2
/// (`session_v2` / `session_message`) **and** V1 (`session` / `message` /
/// `part`, sessions that were never migrated to V2) - with delete-keeping
/// semantics: a committed delete stores the row's last content and stamps
/// `deleted_at_ms` (the delta's observation time); a later insert/update
/// clears the stamp. The extra column is invisible to the V1/V2 readers
/// (they select their known columns only).
const MIRROR_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mirror_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS session_v2 (
    id TEXT PRIMARY KEY,
    title TEXT,
    directory TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    parent_id TEXT,
    deleted_at_ms INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS session_message (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    seq INTEGER,
    type TEXT,
    time_created INTEGER,
    data TEXT,
    deleted_at_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS session_message_session
    ON session_message(session_id, seq);
CREATE TABLE IF NOT EXISTS session (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    title TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    deleted_at_ms INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS message (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    data TEXT,
    deleted_at_ms INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS part (
    id TEXT PRIMARY KEY,
    message_id TEXT,
    session_id TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    data TEXT,
    deleted_at_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS message_session ON message(session_id);
CREATE INDEX IF NOT EXISTS part_session ON part(session_id);
";

#[derive(Debug, Clone)]
struct MirrorState {
    schema: String,
    source_path: String,
    baseline_digest: String,
    last_capture_seq: u64,
    /// Sessions the mirror currently holds rows for.
    sessions: Vec<String>,
}

impl MirrorState {
    fn load(path: &Path) -> Option<MirrorState> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        let get = |key: &str| -> Option<String> {
            conn.query_row(
                "SELECT value FROM mirror_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .ok()
        };
        Some(MirrorState {
            schema: get("schema")?,
            source_path: get("source_path")?,
            baseline_digest: get("baseline_digest")?,
            last_capture_seq: get("last_capture_seq")?.parse().ok()?,
            sessions: get("sessions")
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or_default(),
        })
    }

    /// True when this state describes the given source's current evidence
    /// (same layout, same baseline generation, no pending delta batch).
    fn is_current(&self, archive: &Archive, source_path: &str) -> bool {
        if self.schema != MIRROR_SCHEMA_VERSION || self.source_path != source_path {
            return false;
        }
        let Ok(baselines) = crate::evidence::sqlite_baselines(archive, source_path) else {
            return false;
        };
        let Some(baseline) = baselines
            .iter()
            .find(|baseline| baseline.baseline_digest == self.baseline_digest)
        else {
            return false;
        };
        let max_seq = crate::evidence::sqlite_delta_batch_metas(archive, source_path)
            .map(|metas| {
                metas
                    .iter()
                    .filter(|meta| meta.baseline_generation == baseline.baseline_generation)
                    .map(|meta| meta.capture_seq)
                    .max()
                    .unwrap_or(baseline.capture_seq)
            })
            .unwrap_or(baseline.capture_seq);
        self.last_capture_seq >= max_seq
    }

    fn store(&self, conn: &rusqlite::Connection) -> Result<(), String> {
        conn.execute("DELETE FROM mirror_meta", [])
            .map_err(|error| error.to_string())?;
        let sessions = serde_json::to_string(&self.sessions).unwrap_or_else(|_| "[]".into());
        for (key, value) in [
            ("schema", self.schema.clone()),
            ("source_path", self.source_path.clone()),
            ("baseline_digest", self.baseline_digest.clone()),
            ("last_capture_seq", self.last_capture_seq.to_string()),
            ("sessions", sessions),
        ] {
            conn.execute(
                "INSERT INTO mirror_meta(key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn mirror_column(value: &Value, column: &str) -> rusqlite::types::Value {
    match value.get(column) {
        None | Some(Value::Null) => rusqlite::types::Value::Null,
        Some(Value::String(text)) => rusqlite::types::Value::Text(text.clone()),
        Some(Value::Number(number)) => {
            rusqlite::types::Value::Integer(number.as_i64().unwrap_or(0))
        }
        Some(other) => rusqlite::types::Value::Text(other.to_string()),
    }
}

/// Insert one row into a mirror table. `deleted_at_ms` is `0` for live rows
/// and the deleting delta's observation time for rows kept by a committed
/// delete.
fn insert_mirror_row(
    conn: &rusqlite::Connection,
    table: &str,
    columns: &[&str],
    row: &Value,
    deleted_at_ms: i64,
) -> Result<(), String> {
    let mut values: Vec<rusqlite::types::Value> = columns
        .iter()
        .map(|column| mirror_column(row, column))
        .collect();
    values.push(rusqlite::types::Value::Integer(deleted_at_ms));
    let placeholders: Vec<String> = (1..=values.len()).map(|index| format!("?{index}")).collect();
    let column_list = columns
        .iter()
        .map(|column| format!("\"{column}\""))
        .chain(std::iter::once("\"deleted_at_ms\"".to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT OR REPLACE INTO {table} ({column_list}) VALUES ({})",
        placeholders.join(", ")
    );
    conn.execute(&sql, rusqlite::params_from_iter(values.iter()))
        .map_err(|error| format!("insert {table}: {error}"))?;
    Ok(())
}

const SESSION_V2_COLUMNS: [&str; 6] = [
    "id",
    "title",
    "directory",
    "time_created",
    "time_updated",
    "parent_id",
];
const SESSION_MESSAGE_COLUMNS: [&str; 6] = [
    "id",
    "session_id",
    "seq",
    "type",
    "time_created",
    "data",
];
const V1_SESSION_COLUMNS: [&str; 5] = [
    "id",
    "parent_id",
    "title",
    "time_created",
    "time_updated",
];
const V1_MESSAGE_COLUMNS: [&str; 5] = [
    "id",
    "session_id",
    "time_created",
    "time_updated",
    "data",
];
const V1_PART_COLUMNS: [&str; 6] = [
    "id",
    "message_id",
    "session_id",
    "time_created",
    "time_updated",
    "data",
];

/// The session-bearing whitelist tables the mirror retains, in parent-first
/// insert order.
fn mirror_table_columns(table: &str) -> Option<&'static [&'static str]> {
    match table {
        "session_v2" => Some(SESSION_V2_COLUMNS.as_slice()),
        "session_message" => Some(SESSION_MESSAGE_COLUMNS.as_slice()),
        "session" => Some(V1_SESSION_COLUMNS.as_slice()),
        "message" => Some(V1_MESSAGE_COLUMNS.as_slice()),
        "part" => Some(V1_PART_COLUMNS.as_slice()),
        _ => None,
    }
}

/// Replay one SQLite evidence source into `conn` (which must already carry
/// [`MIRROR_SCHEMA`]) with **delete-keeping semantics**: a committed delete
/// stores the `before` row content and stamps `deleted_at_ms` (the delta's
/// observation time) instead of removing the row; a later insert/update
/// stores the new content and clears the stamp. Returns the final capture
/// seq. Only the session-bearing whitelist tables are decompressed and
/// retained; the nearest baseline is loaded from its chunks and the deltas of
/// its generation are applied in capture order (the unfiltered replay's
/// bookkeeping).
/// Apply one committed delta line under delete-keeping semantics: a delete
/// keeps the `before` row content and stamps `deleted_at_ms` (the delta's
/// observation time); insert/update stores the new content and clears the
/// stamp. A delete without a `before` image drops the row (nothing
/// recoverable).
fn apply_delta_line_keep_deletes(
    conn: &rusqlite::Connection,
    line: &crate::evidence::DeltaLine,
) -> Result<(), String> {
    let Some(columns) = mirror_table_columns(&line.table) else {
        return Ok(());
    };
    if line.op == "delete" {
        match line.before.as_ref() {
            Some(before) => {
                insert_mirror_row(conn, &line.table, columns, before, line.observed_at_ms)
            }
            None => {
                let id = row_id_from_key(&line.key);
                conn.execute(
                    &format!("DELETE FROM {} WHERE id = ?1", line.table),
                    rusqlite::params![id],
                )
                .map_err(|error| error.to_string())?;
                Ok(())
            }
        }
    } else if let Some(after) = line.after.as_ref() {
        insert_mirror_row(conn, &line.table, columns, after, 0)
    } else {
        Ok(())
    }
}

/// Tunes a freshly created mirror/temp database for bulk rebuild. The mirror
/// is a deletable, rebuildable cache: durability across a crash mid-rebuild
/// is worthless (the staging file is discarded and rebuilt), so the rollback
/// journal and fsyncs are switched off. ~1000x on bulk inserts (verifier B4).
fn tune_rebuildable_cache(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF;")
        .map_err(|error| error.to_string())
}

/// Insert the baseline rows of the **wanted sessions only** (rework 3: the
/// mirror is on-demand). Child-table chunks are decompressed and filtered by
/// the row's `session_id`; session-table chunks by the row's `id`. Memory
/// stays at one chunk; the mirror only ever holds wanted sessions' rows.
fn insert_baseline_session_rows(
    batch: &EvidenceBatch<'_>,
    baseline: &crate::evidence::SqliteBaseline,
    conn: &rusqlite::Connection,
    wanted: &BTreeSet<String>,
) -> Result<(), String> {
    let fetch = batch.read_objects();
    for chunk in &baseline.chunks {
        let Some(columns) = mirror_table_columns(&chunk.table) else {
            continue;
        };
        // Session tables (V2 `session_v2` and V1 `session`) key by `id`;
        // child tables link by `session_id`.
        let by_id = is_session_table(&chunk.table);
        for line in crate::evidence::read_chunk_rows_fetch(&fetch, chunk)
            .map_err(|error| error.to_string())?
        {
            if row_matches_wanted(&line.row, by_id, wanted) {
                insert_mirror_row(conn, &chunk.table, columns, &line.row, 0)?;
            }
        }
    }
    Ok(())
}

/// Apply the committed delta batches of one baseline generation in
/// `(after_capture_seq, upto_capture_seq]` under delete-keeping semantics,
/// filtered to the wanted sessions (session-table lines by key id; child-table
/// lines by the row's `session_id`). Lines are content-complete
/// (`before`/`after`), so re-applying an already-applied batch is idempotent -
/// the growth path reuses this to backfill newly wanted sessions from the
/// baseline capture seq without re-touching known sessions.
fn apply_session_deltas(
    batch: &EvidenceBatch<'_>,
    source_path: &str,
    conn: &rusqlite::Connection,
    wanted: &BTreeSet<String>,
    baseline_generation: u64,
    after_capture_seq: u64,
    upto_capture_seq: u64,
) -> Result<u64, String> {
    let mut capture_seq = after_capture_seq;
    for meta in crate::evidence::sqlite_delta_batch_metas_in(batch.manifests, source_path) {
        if meta.baseline_generation != baseline_generation
            || meta.capture_seq <= after_capture_seq
            || meta.capture_seq > upto_capture_seq
        {
            continue;
        }
        let fetch = batch.read_objects();
        for line in crate::evidence::read_delta_lines_fetch(&fetch, &meta)
            .map_err(|error| error.to_string())?
        {
            if !delta_line_matches_wanted(&line, wanted) {
                continue;
            }
            apply_delta_line_keep_deletes(conn, &line)?;
        }
        capture_seq = meta.capture_seq;
    }
    Ok(capture_seq)
}

fn is_session_table(table: &str) -> bool {
    matches!(table, "session_v2" | "session")
}

/// Does this row belong to a wanted session? `by_id`: session tables match
/// by `id` (the key); child tables by the row's `session_id` link column.
fn row_matches_wanted(row: &Value, by_id: bool, wanted: &BTreeSet<String>) -> bool {
    if by_id {
        row.get("id").and_then(Value::as_str)
    } else {
        row.get("session_id").and_then(Value::as_str)
    }
    .is_some_and(|id| wanted.contains(id))
}

/// Same filter for one committed delta line (delete lines carry their session
/// link in `before`; insert/update in `after`).
fn delta_line_matches_wanted(
    line: &crate::evidence::DeltaLine,
    wanted: &BTreeSet<String>,
) -> bool {
    if !mirror_table_columns(&line.table).is_some() {
        return false;
    }
    if is_session_table(&line.table) {
        return wanted.contains(&row_id_from_key(&line.key));
    }
    let row = line.after.as_ref().or(line.before.as_ref());
    match row {
        Some(row) => row
            .get("session_id")
            .and_then(Value::as_str)
            .is_some_and(|session| wanted.contains(session)),
        None => false,
    }
}

/// Delta-line keys are `pk:["<id>"]` (canonical primary-key JSON); the id is
/// the first JSON string inside the brackets.
fn row_id_from_key(key: &str) -> String {
    let inner = key
        .strip_prefix("pk:[")
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or("");
    let trimmed = inner.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    unquoted
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn opencode_rel_path(source: &SourceConfig) -> String {
    match source.db.as_deref() {
        Some(db) => rel_from_root(&source.root, Path::new(db)).unwrap_or_else(|| {
            Path::new(db)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        }),
        None => "opencode.db".to_string(),
    }
}

fn archive_locator(deleted_at_ms: i64) -> crate::recall::refs::ArchiveLocator {
    crate::recall::refs::ArchiveLocator {
        record_sha256: None,
        deleted_at_ms: if deleted_at_ms > 0 {
            Some(deleted_at_ms)
        } else {
            None
        },
    }
}

fn remember_session_row(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    title: &str,
    session_kind: &'static str,
) -> Result<(), String> {
    tx.execute(
        "INSERT OR REPLACE INTO sessions(session_id, title, session_kind) VALUES (?1, ?2, ?3)",
        rusqlite::params![session_id, title, session_kind],
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// Build units for one archived V2 session from the mirror and insert them as
/// archive rows (plus the `sessions` row). `deleted_at_ms > 0` marks a session
/// the user deleted in OpenCode: the rows were kept by the mirror's
/// delete-keeping replay and the deletion time rides on every hit.
fn index_session_rows(
    source: &SourceConfig,
    index: &MirrorReader,
    session_id: &str,
    tx: &rusqlite::Transaction<'_>,
    writer: &mut Writer<'_>,
) -> Result<u64, String> {
    let mut added = 0u64;
    let session = index.session(session_id)?;
    let session_kind =
        super::opencode_session_kind(session.parent_id.as_deref(), Some(&session.title));
    let title = if session.title.is_empty() {
        session_id.to_string()
    } else {
        session.title.clone()
    };
    remember_session_row(tx, session_id, &title, session_kind)?;
    let rel_path = opencode_rel_path(source);
    let marker = archive_locator(session.deleted_at_ms);
    for row in &session.rows {
        if row.data.len() as u64 > crate::recall::port::sqlite_v2::MAX_ROW_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&row.data) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        let mut units = super::build::opencode_units(
            session_id,
            &row.id,
            &title,
            session_kind,
            &rel_path,
            row.time_created,
            &row.message_type,
            &row.row_sha256,
            &value,
        );
        if units.is_empty() {
            continue;
        }
        for unit in &mut units {
            unit.origin = ORIGIN_ARCHIVE;
            unit.locator.archive_fallback = Some(marker.clone());
        }
        for unit in &units {
            writer.insert(unit).map_err(|error| error.to_string())?;
            added += 1;
        }
    }
    Ok(added)
}

/// Build units for one archived **V1** session (`session`/`message`/`part` —
/// sessions that were never migrated to V2 and are invisible to the live V2
/// index). The part records come from the ported V1 reader (the scan/readback
/// provider), the role rule and the block projection are the existing
/// provider/OpenCode-row logic; the locator is the V1 shape
/// (`session_id` + `message_id` + `part_id`) that `memory_read` resolves back
/// through the same reader.
fn index_v1_session_rows(
    source: &SourceConfig,
    mirror_path: &Path,
    session_id: &str,
    tx: &rusqlite::Transaction<'_>,
    writer: &mut Writer<'_>,
) -> Result<u64, String> {
    let mut added = 0u64;
    let Some(info) = MirrorReader::open(mirror_path)?.v1_session(session_id)? else {
        return Ok(0);
    };
    let session_kind =
        super::opencode_session_kind(info.parent_id.as_deref(), Some(&info.title));
    let title = if info.title.is_empty() {
        session_id.to_string()
    } else {
        info.title.clone()
    };
    remember_session_row(tx, session_id, &title, session_kind)?;
    let rel_path = opencode_rel_path(source);
    let marker = archive_locator(info.deleted_at_ms);

    // The provider's role rule (`message.data.role`, missing -> unknown).
    let roles = {
        let conn = rusqlite::Connection::open_with_flags(
            mirror_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|error| error.to_string())?;
        crate::providers::opencode::sqlite_v1_message_roles(&conn, session_id)?
    };
    let reader = crate::recall::port::sqlite_v1::V1Reader::open(mirror_path, &rel_path)
        .map_err(|error| error.to_string())?;
    let mut health = crate::recall::port::Health::default();
    let parts =
        reader.parts_for_session(session_id, crate::recall::port::sqlite_v2::MAX_ROW_BYTES, &mut health)
            .map_err(|error| error.to_string())?;
    for part in parts {
        let PartData::Block(block) = &part.data else {
            continue;
        };
        let role = match roles
            .get(part.locator.message_id.as_deref().unwrap_or_default())
            .map(String::as_str)
        {
            Some("user") => crate::recall::contract::ROLE_USER,
            _ => crate::recall::contract::ROLE_ASSISTANT,
        };
        let outcome = crate::recall::port::classify_part(&part.data);
        let mut units = super::build::opencode_block_units(
            session_id,
            &title,
            session_kind,
            &rel_path,
            part.time_created,
            role,
            &part.row_sha256,
            part.locator.clone(),
            block,
            outcome,
        );
        if units.is_empty() {
            continue;
        }
        for unit in &mut units {
            unit.origin = ORIGIN_ARCHIVE;
            unit.locator.archive_fallback = Some(marker.clone());
        }
        for unit in &units {
            writer.insert(unit).map_err(|error| error.to_string())?;
            added += 1;
        }
    }
    Ok(added)
}

// ---------------------------------------------------------------------------
// Read side (memory_read; never writes the cache root)
// ---------------------------------------------------------------------------

/// Opened evidence for fallback readback.
pub struct ReadEvidence {
    archive: Archive,
    domain: String,
    cache_root: Option<PathBuf>,
}

impl ReadEvidence {
    pub fn open(config: &RecallConfig) -> Result<ReadEvidence, RecallError> {
        let root = configured_archive_root(config).ok_or_else(|| {
            RecallError::new(
                codes::DB_UNAVAILABLE,
                "archive-fallback ref has no configured archive source (fallback_archive_missing)",
            )
        })?;
        let archive = archive_provider::open(Path::new(&root))
            .map_err(|failure| RecallError::new(failure.code, failure.message.clone()))?;
        Ok(ReadEvidence {
            archive,
            domain: config.effective_domain(None),
            cache_root: config.cache_root.clone().map(PathBuf::from),
        })
    }

    /// Find the committed carrier group matching `rel_path` inside the live
    /// root and rebuild its logical bytes. When `record_sha256` is given, the
    /// record must be committed in exactly that group (§3.2 carrier check).
    pub fn rebuild_jsonl_carrier(
        &self,
        live_root: &str,
        rel_path: &str,
        record_sha256: Option<&str>,
    ) -> Result<Vec<u8>, RecallError> {
        let manifests = self
            .archive
            .manifests()
            .map_err(|error| read_failed(error.to_string()))?;
        let mut by_source: BTreeMap<&str, Vec<&Manifest>> = BTreeMap::new();
        for manifest in manifests.iter() {
            by_source
                .entry(manifest.source.path.as_str())
                .or_default()
                .push(manifest);
        }
        for (path, group) in &by_source {
            let kind = group
                .last()
                .map(|manifest| manifest.source.kind.clone())
                .unwrap_or_default();
            if !matches!(
                kind.as_str(),
                "jsonl_file" | "compressed_jsonl_file" | "whole_file" | "dsh_jsonl"
            ) {
                continue;
            }
            let rel = if kind == "dsh_jsonl" {
                // A DSH group's `path` is the session directory; the ref names
                // the generation carrier recorded by the capture, so the live
                // root's current location does not matter here.
                match group.last().and_then(|m| m.source.carrier.clone()) {
                    Some(carrier) => carrier,
                    None => continue,
                }
            } else {
                let Some(rel) = carrier_rel(live_root, path, &kind) else {
                    continue;
                };
                rel
            };
            if !rel.eq_ignore_ascii_case(rel_path) {
                continue;
            }
            let selected = select_group_manifests(group);
            if let Some(record_sha) = record_sha256 {
                let present = selected
                    .iter()
                    .any(|manifest| manifest.records.iter().any(|record| record.sha256 == record_sha));
                if !present {
                    return Err(RecallError::new(
                        codes::REF_LOCATOR_MISSING,
                        "archive record is not committed in the carrier named by rel_path",
                    ));
                }
            }
            let snapshot = EvidenceSnapshot::load(&self.archive).map_err(read_failed)?;
            let batch = EvidenceBatch::new(&self.archive, &snapshot, &self.domain);
            return rebuild_jsonl_bytes(&batch, &selected).map_err(read_failed);
        }
        Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "no archived carrier matches ref.rel_path",
        ))
    }

    /// The database a fallback ref resolves against: the cached row mirror
    /// when it is current and holds the session (opened read-only — the read
    /// path never writes the cache root), otherwise a **temporary**
    /// delete-keeping materialization that the caller removes afterwards.
    /// Returns the database path plus the temp file to clean up (`None` for
    /// the shared mirror).
    fn evidence_db(
        &self,
        session_id: &str,
        temp: &Path,
    ) -> Result<(PathBuf, Option<PathBuf>), RecallError> {
        if let Some(cache_root) = &self.cache_root {
            let dir = cache_root.join("archive-inventory").join("rows");
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut paths: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("sqlite"))
                    .collect();
                paths.sort();
                for mirror in paths {
                    let Some(state) = MirrorState::load(&mirror) else {
                        continue;
                    };
                    if !state.is_current(&self.archive, &state.source_path) {
                        continue;
                    }
                    let Ok(index) = MirrorReader::open(&mirror) else {
                        continue;
                    };
                    if index.has_session(session_id) {
                        return Ok((mirror, None));
                    }
                }
            }
        }

        // No current mirror holds the session: materialize the evidence state
        // (delete-keeping, V1+V2 tables) into the temporary file per source.
        let manifests = self
            .archive
            .manifests()
            .map_err(|error| read_failed(error.to_string()))?;
        let mut sqlite_sources: Vec<String> = Vec::new();
        for manifest in manifests.iter() {
            if manifest.source.kind == "sqlite"
                && !sqlite_sources.iter().any(|path| path == &manifest.source.path)
            {
                sqlite_sources.push(manifest.source.path.clone());
            }
        }
        sqlite_sources.sort();
        let snapshot = EvidenceSnapshot::load(&self.archive).map_err(read_failed)?;
        for source_path in &sqlite_sources {
            let _ = std::fs::remove_file(temp);
            let created = rusqlite::Connection::open(temp)
                .map_err(|error| read_failed(format!("materialize: {error}")))
                .and_then(|conn| {
                    (|| {
                        tune_rebuildable_cache(&conn)?;
                        conn.execute_batch(MIRROR_SCHEMA)
                            .map_err(|error| error.to_string())?;
                        // Per-session extraction (rework 3): decompress the
                        // generation's chunks/batches once, keep only this
                        // session's rows - never the whole whitelist payload.
                        let batch =
                            EvidenceBatch::new(&self.archive, &snapshot, &self.domain);
                        let baselines =
                            crate::evidence::sqlite_baselines_in(batch.manifests, source_path);
                        let baseline = baselines
                            .last()
                            .ok_or_else(|| "no SQLite baseline committed".to_string())?;
                        let wanted: BTreeSet<String> =
                            [session_id.to_string()].into_iter().collect();
                        insert_baseline_session_rows(&batch, baseline, &conn, &wanted)?;
                        let max_seq = crate::evidence::sqlite_delta_batch_metas_in(
                            batch.manifests,
                            source_path,
                        )
                        .iter()
                        .filter(|meta| meta.baseline_generation == baseline.baseline_generation)
                        .map(|meta| meta.capture_seq)
                        .max()
                        .unwrap_or(baseline.capture_seq);
                        if max_seq > baseline.capture_seq {
                            apply_session_deltas(
                                &batch,
                                source_path,
                                &conn,
                                &wanted,
                                baseline.baseline_generation,
                                baseline.capture_seq,
                                max_seq,
                            )?;
                        }
                        let index = MirrorReader::open(temp).map_err(|error| error.to_string())?;
                        Ok::<bool, String>(index.has_session(session_id))
                    })()
                    .map_err(read_failed)
                });
            match created {
                Ok(true) => return Ok((temp.to_path_buf(), Some(temp.to_path_buf()))),
                _ => {
                    let _ = std::fs::remove_file(temp);
                }
            }
        }
        Err(RecallError::new(
            codes::REF_LOCATOR_MISSING,
            "session is not present in any archived SQLite source",
        ))
    }
}

fn read_failed(message: impl Into<String>) -> RecallError {
    RecallError::new(codes::REF_READ_FAILED, message.into())
}

/// Route an archive-fallback `memory_read` request into the evidence.
pub fn read_fallback(
    config: &RecallConfig,
    source: &SourceConfig,
    request: &crate::recall::read::ReadRequest,
) -> Result<crate::recall::read::ReadOutcome, RecallError> {
    let evidence = ReadEvidence::open(config)?;
    match source.kind {
        SourceKind::Opencode => evidence.read_opencode(source, request),
        SourceKind::Codex => evidence.read_codex(source, request),
        SourceKind::ClaudeCode => evidence.read_claude(source, request),
        SourceKind::Dsh => evidence.read_dsh(source, request),
        _ => Err(RecallError::new(
            codes::REF_SOURCE_MISMATCH,
            "archive-fallback refs exist only for opencode, codex, claude_code and dsh sources",
        )),
    }
}

impl ReadEvidence {
    fn read_opencode(
        &self,
        source: &SourceConfig,
        request: &crate::recall::read::ReadRequest,
    ) -> Result<crate::recall::read::ReadOutcome, RecallError> {
        let _ = source;
        let reference = &request.reference;
        if !matches!(
            reference.schema_gen.as_str(),
            SCHEMA_SQLITE_V2 | crate::recall::contract::SCHEMA_SQLITE_V1
        ) {
            return Err(RecallError::new(
                codes::REF_SOURCE_MISMATCH,
                "archive-fallback opencode refs use sqlite-v2 or sqlite-v1",
            ));
        }
        let session_id = reference
            .locator
            .session_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                RecallError::new(
                    codes::REF_LOCATOR_MISSING,
                    "fallback opencode locator requires a session_id",
                )
            })?;
        let (temp, temp_dir) = fallback_temp_file("opencode-readback", "db")?;
        let result = (|| {
            let (db, _owned) = self.evidence_db(session_id, &temp)?;
            let outcome = match reference.schema_gen.as_str() {
                crate::recall::contract::SCHEMA_SQLITE_V1 => {
                    let reader = crate::recall::port::sqlite_v1::V1Reader::open(
                        &db,
                        &reference.rel_path,
                    )
                    .map_err(|error| {
                        RecallError::new(
                            crate::recall::providers::port_code(&error),
                            error.message(),
                        )
                    })?;
                    let mut wrapped = crate::recall::port::reader::Reader::V1(reader);
                    crate::recall::read::read_parts_after_resolve(&mut wrapped, request)
                }
                _ => {
                    let parent_id_available = sqlite_parent_id_available(&db);
                    let reader = crate::recall::port::sqlite_v2::V2Reader::open(
                        &db,
                        &reference.rel_path,
                        parent_id_available,
                    )
                    .map_err(|error| {
                        RecallError::new(
                            crate::recall::providers::port_code(&error),
                            error.message(),
                        )
                    })?;
                    let mut wrapped = crate::recall::port::reader::Reader::V2(reader);
                    crate::recall::read::read_parts_after_resolve(&mut wrapped, request)
                }
            };
            outcome
        })();
        // Always clean the readback temp file and its dedicated directory -
        // including the mirror-hit path where no temp file was ever created
        // (N1: no leftover `s5-readback-*` directories, success or failure).
        cleanup_readback_temp(&temp, &temp_dir);
        result
    }

    fn read_codex(
        &self,
        source: &SourceConfig,
        request: &crate::recall::read::ReadRequest,
    ) -> Result<crate::recall::read::ReadOutcome, RecallError> {
        let reference = &request.reference;
        let record_sha = reference
            .locator
            .archive_fallback
            .as_ref()
            .and_then(|archive| archive.record_sha256.clone());
        let bytes =
            self.rebuild_jsonl_carrier(&source.root, &reference.rel_path, record_sha.as_deref())?;
        crate::recall::read::scan_codex_stream(Cursor::new(bytes), request)
    }

    fn read_claude(
        &self,
        source: &SourceConfig,
        request: &crate::recall::read::ReadRequest,
    ) -> Result<crate::recall::read::ReadOutcome, RecallError> {
        let reference = &request.reference;
        let record_sha = reference
            .locator
            .archive_fallback
            .as_ref()
            .and_then(|archive| archive.record_sha256.clone());
        let bytes =
            self.rebuild_jsonl_carrier(&source.root, &reference.rel_path, record_sha.as_deref())?;
        crate::recall::read::scan_claude_stream(Cursor::new(bytes), request)
    }

    /// A DSH archive-fallback ref names a carrier relative path and a logical
    /// line; the committed logical ranges are re-spliced and the same DSH
    /// scanner runs over them.
    fn read_dsh(
        &self,
        source: &SourceConfig,
        request: &crate::recall::read::ReadRequest,
    ) -> Result<crate::recall::read::ReadOutcome, RecallError> {
        let reference = &request.reference;
        if reference.schema_gen != SCHEMA_DSH_JSONL {
            return Err(RecallError::new(
                codes::REF_SOURCE_MISMATCH,
                "archive-fallback dsh refs use dsh-jsonl",
            ));
        }
        let record_sha = reference
            .locator
            .archive_fallback
            .as_ref()
            .and_then(|archive| archive.record_sha256.clone());
        let bytes =
            self.rebuild_jsonl_carrier(&source.root, &reference.rel_path, record_sha.as_deref())?;
        let log = dsh::log_from_logical(&reference.rel_path, bytes);
        crate::recall::read::scan_dsh_log(&log, request)
    }
}

fn sqlite_parent_id_available(path: &Path) -> bool {
    crate::recall::port::probe::open_readonly(path)
        .ok()
        .and_then(|conn| crate::recall::port::probe::table_columns(&conn, "session_v2").ok())
        .flatten()
        .map(|columns| columns.contains("parent_id"))
        .unwrap_or(false)
}

/// A private temporary file for fallback readback materialization. The cache
/// root is never touched (MCP stays read-only); the process's temporary
/// directory holds a dedicated per-read subdirectory and the caller removes
/// file and directory afterwards. Returns `(file, directory)`.
fn fallback_temp_file(prefix: &str, suffix: &str) -> Result<(PathBuf, PathBuf), RecallError> {
    let dir = std::env::temp_dir().join(format!(
        "s5-readback-{}-{}",
        std::process::id(),
        super::now_ms()
    ));
    std::fs::create_dir_all(&dir).map_err(|error| read_failed(format!("temp dir: {error}")))?;
    let name = format!(
        "{}-{}.{}",
        prefix,
        &sha256_hex(dir.as_os_str().to_string_lossy().as_bytes())[..12],
        suffix
    );
    let file = dir.join(name);
    Ok((file, dir))
}

/// Remove a readback's temporary file and its dedicated directory. Removal
/// failures are reported on stderr (never swallowed silently) and never fail
/// the read: the file lives in the OS temp area either way.
fn cleanup_readback_temp(file: &Path, dir: &Path) {
    if let Err(error) = std::fs::remove_file(file) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!("agent-memory: readback temp file {}: {error}", file.display());
        }
    }
    if let Err(error) = std::fs::remove_dir_all(dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "agent-memory: readback temp dir {}: {error}",
                dir.display()
            );
        }
    }
}
