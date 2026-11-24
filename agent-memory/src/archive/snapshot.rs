//! One manifest snapshot per archive instance: the committed manifest set plus
//! the object/record access indexes derived from it.
//!
//! # Why
//!
//! Every readback used to re-parse the whole manifest set: `object_accessible`
//! called `manifests()` once per object read, so a capture that read N evidence
//! objects paid `N x manifests`. On the production store (≈5 300 manifests,
//! ≈207 MB) that is the difference between 45 MB and 15 GB of reads for one
//! incremental round.
//!
//! # Invalidation
//!
//! The snapshot is keyed on the **manifest directory listing**: the sorted file
//! names *and* each file's `(size, mtime)`. The rule:
//!
//! - the listing is unchanged → reuse the snapshot, parse nothing;
//! - the listing is the previous listing plus new entries at the end → parse
//!   only the new manifests and extend the snapshot (manifests are append-only,
//!   so this is the normal path: this process's own `commit_manifest`, or
//!   another process's commit);
//! - anything else (a name disappeared, a file was rewritten, the order
//!   changed) → rebuild from scratch, so a manifest edited behind the snapshot
//!   is re-read instead of served stale.
//!
//! A rewrite that keeps both the size and the modification time is not
//! detected (the same limit the capture-all skip ledger has); the recovery is
//! the documented one for that case — rewrite it so a stamp moves, or drop the
//! cache by starting a new process.
//!
//! # Build path
//!
//! One build is one directory listing, one read+parse per manifest file, and one
//! table insert per committed object reference. Two properties keep it cheap on a
//! store of thousands of manifests:
//!
//! - the manifest bodies are independent, so a large set is read and parsed on
//!   scoped worker threads and merged back in listing order (the result — and the
//!   first error — is the sequential one);
//! - the record index is built **on demand**, on the first record access check.
//!   A capture round reads objects, so it never pays for the record side of the
//!   index.
//!
//! # Access control
//!
//! The indexes replay the *same* decision as the original per-read scan, in the
//! same order (manifest order) and with the same fail-closed rules: an empty
//! access tag list denies, a tag list without the domain denies, and an object
//! or record with no committed reference denies. Only the scan is replaced by a
//! lookup of the references for that one sha.
//!
//! Both indexes are open-addressed multimaps keyed by the 64-bit FNV-1a hash of
//! the sha ([`RefTable`]): one slot per committed reference, so inserting a
//! reference allocates nothing and copies no key, and candidate references come
//! back in insertion order, which is manifest order. When two shas share a hash,
//! the caller compares the candidate's sha with the requested one, so a collision
//! costs work but never changes a decision.

use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use super::{ArchiveError, Manifest, RecordRef, MANIFEST_PARSES};

/// Size + modification time of one manifest file, captured from the directory
/// listing. A change in either means the body must be re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub mtime_ms: i64,
}

/// One manifest file of the listing.
#[derive(Debug, Clone)]
pub struct ManifestFile {
    pub name: String,
    pub path: PathBuf,
    pub stamp: FileStamp,
}

/// One committed reference to an object or record: which manifest, and which
/// entry inside it. The access tags are read from the manifest itself, so the
/// index never duplicates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct EntryRef {
    manifest: u32,
    entry: u32,
}

// ---------------------------------------------------------------------------
// Snapshot build counters
// ---------------------------------------------------------------------------

/// Process-wide build/refresh counters. They are the mechanical evidence that a
/// round lists and builds the manifest set a bounded number of times; see
/// [`snapshot_stats`].
static LISTS: AtomicU64 = AtomicU64::new(0);
static BUILDS: AtomicU64 = AtomicU64::new(0);
static EXTENSIONS: AtomicU64 = AtomicU64::new(0);
static COPIES: AtomicU64 = AtomicU64::new(0);
static OBJECT_REFS: AtomicU64 = AtomicU64::new(0);
static RECORD_REFS: AtomicU64 = AtomicU64::new(0);
static RECORD_INDEX_BUILDS: AtomicU64 = AtomicU64::new(0);
static INSERT_PROBES: AtomicU64 = AtomicU64::new(0);

/// Process-wide snapshot counters (see [`snapshot_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Manifest directory listings (one per `Archive::manifests` refresh).
    pub lists: u64,
    /// Full builds from scratch (fresh parse of the whole manifest set).
    pub builds: u64,
    /// Append-only extensions (parse of the new manifests only).
    pub extensions: u64,
    /// Extensions that had to copy a snapshot another handle still uses.
    pub copies: u64,
    /// Object references indexed (one table insert each).
    pub object_refs: u64,
    /// Record references indexed (record index builds only).
    pub record_refs: u64,
    /// Lazy record index builds (one per snapshot instance that checks a record).
    pub record_index_builds: u64,
    /// Table slots probed while inserting: linear evidence (`probes <= 2 x refs`).
    pub insert_probes: u64,
}

/// Current values of the snapshot build counters (read-only diagnostics).
pub fn snapshot_stats() -> SnapshotStats {
    SnapshotStats {
        lists: LISTS.load(Ordering::Relaxed),
        builds: BUILDS.load(Ordering::Relaxed),
        extensions: EXTENSIONS.load(Ordering::Relaxed),
        copies: COPIES.load(Ordering::Relaxed),
        object_refs: OBJECT_REFS.load(Ordering::Relaxed),
        record_refs: RECORD_REFS.load(Ordering::Relaxed),
        record_index_builds: RECORD_INDEX_BUILDS.load(Ordering::Relaxed),
        insert_probes: INSERT_PROBES.load(Ordering::Relaxed),
    }
}

/// Count one manifest directory listing (called by [`super::Archive::manifests`]).
pub(crate) fn note_list() {
    LISTS.fetch_add(1, Ordering::Relaxed);
}

/// Count one append-only snapshot extension.
pub(crate) fn note_extension() {
    EXTENSIONS.fetch_add(1, Ordering::Relaxed);
}

/// Count one snapshot copy (`Arc::try_unwrap` failed: another handle still holds
/// the previous snapshot).
pub(crate) fn note_copy() {
    COPIES.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Reference table
// ---------------------------------------------------------------------------

/// One table slot: `hash == 0` marks a free slot (a sha hash is never 0).
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    hash: u64,
    manifest: u32,
    entry: u32,
}

/// Free slots in a fresh table. Also guarantees a non-empty slot slice, which the
/// probe loop relies on.
const MIN_SLOTS: usize = 64;

/// Open-addressed multimap from a sha to every committed reference to it.
///
/// - one slot per reference: inserting a reference allocates nothing and copies
///   no key (the sha is hashed, not stored);
/// - a lookup probes from the sha hash to the first free slot, which yields every
///   reference of that sha (a resize re-probes in slot order, so the table does
///   not promise an order: callers sort by manifest/entry, which is the scan
///   order the access decision needs);
/// - two shas that share a hash land in the same probe run: the caller compares
///   the candidate's sha with the requested one, so a collision can only cost
///   work, never change a decision;
/// - `len * 2 <= slots.len()` is maintained, so a probe run always ends and an
///   insert probes a bounded number of slots.
#[derive(Debug, Clone)]
struct RefTable {
    slots: Vec<Slot>,
    len: usize,
}

impl Default for RefTable {
    fn default() -> RefTable {
        RefTable::new()
    }
}

impl RefTable {
    fn new() -> RefTable {
        RefTable {
            slots: vec![Slot::default(); MIN_SLOTS],
            len: 0,
        }
    }

    /// Reserve room for `extra` more references (the load factor stays at or
    /// below one half, so insert probes stay bounded).
    fn reserve(&mut self, extra: usize) {
        let wanted = self
            .len
            .saturating_add(extra)
            .saturating_mul(2)
            .next_power_of_two()
            .max(MIN_SLOTS);
        if wanted > self.slots.len() {
            self.resize(wanted);
        }
    }

    fn resize(&mut self, slots: usize) {
        let mut bigger = vec![Slot::default(); slots];
        let mask = slots - 1;
        // Every stored slot keeps its hash, so a resize only re-probes; it never
        // re-hashes a key.
        for slot in self.slots.iter().filter(|slot| slot.hash != 0) {
            let mut index = (slot.hash as usize) & mask;
            while bigger[index].hash != 0 {
                index = (index + 1) & mask;
            }
            bigger[index] = *slot;
        }
        self.slots = bigger;
    }

    /// Insert one reference; returns the number of occupied slots it probed.
    fn insert(&mut self, sha: &str, reference: EntryRef) -> u64 {
        self.insert_hashed(sha_hash(sha), reference)
    }

    /// Insert one reference under an explicit hash (the seam the collision test
    /// uses to force two shas into one probe run).
    fn insert_hashed(&mut self, hash: u64, reference: EntryRef) -> u64 {
        if (self.len + 1) * 2 > self.slots.len() {
            self.resize(self.slots.len() * 2);
        }
        let mask = self.slots.len() - 1;
        let mut index = (hash as usize) & mask;
        let mut probes = 0;
        while self.slots[index].hash != 0 {
            probes += 1;
            index = (index + 1) & mask;
        }
        self.slots[index] = Slot {
            hash,
            manifest: reference.manifest,
            entry: reference.entry,
        };
        self.len += 1;
        probes
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }

    /// Candidate references for `sha`, in no particular order. A candidate may
    /// belong to a different sha that shares the hash; callers compare the sha.
    fn candidates(&self, sha: &str) -> Candidates<'_> {
        self.candidates_hashed(sha_hash(sha))
    }

    fn candidates_hashed(&self, hash: u64) -> Candidates<'_> {
        let mask = self.slots.len() - 1;
        Candidates {
            slots: &self.slots,
            mask,
            hash,
            index: (hash as usize) & mask,
        }
    }
}

struct Candidates<'a> {
    slots: &'a [Slot],
    mask: usize,
    hash: u64,
    index: usize,
}

impl Iterator for Candidates<'_> {
    type Item = EntryRef;

    fn next(&mut self) -> Option<EntryRef> {
        loop {
            let slot = self.slots[self.index];
            // A free slot ends the probe run: nothing inserted for this hash can
            // live past it.
            if slot.hash == 0 {
                return None;
            }
            self.index = (self.index + 1) & self.mask;
            if slot.hash == self.hash {
                return Some(EntryRef {
                    manifest: slot.manifest,
                    entry: slot.entry,
                });
            }
        }
    }
}

/// 64-bit FNV-1a of a sha string. Any deterministic hash works: a lookup verifies
/// the candidate sha, so only the work distribution depends on it. The value 0 is
/// reserved for the "free slot" marker.
fn sha_hash(sha: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in sha.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if hash == 0 {
        1
    } else {
        hash
    }
}

// ---------------------------------------------------------------------------
// Reading the manifest files
// ---------------------------------------------------------------------------

/// Files below this count are parsed on the calling thread: the thread hand-off
/// would cost more than the reads.
const PARALLEL_MIN_FILES: usize = 32;

/// Upper bound on parse workers. The work is I/O plus serde; more workers only
/// add memory pressure.
const MAX_PARSE_WORKERS: usize = 8;

/// Read and parse every manifest body, in file order.
///
/// A large set is parsed on scoped worker threads (the files are independent) and
/// merged back in listing order, so the snapshot content and the reported error
/// are the ones a sequential read produces: the error of the lowest file index.
fn parse_manifests(files: &[ManifestFile]) -> Result<Vec<Manifest>, ArchiveError> {
    let workers = if files.len() < PARALLEL_MIN_FILES {
        1
    } else {
        std::thread::available_parallelism()
            .map(|cores| cores.get())
            .unwrap_or(1)
            .min(MAX_PARSE_WORKERS)
            .min(files.len())
    };
    if workers <= 1 {
        let mut parsed = Vec::with_capacity(files.len());
        for file in files {
            parsed.push(parse_one(file)?);
        }
        return Ok(parsed);
    }
    let chunk = files.len().div_ceil(workers);
    let mut slots: Vec<Option<Result<Manifest, ArchiveError>>> =
        (0..files.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (part, out) in files.chunks(chunk).zip(slots.chunks_mut(chunk)) {
            handles.push(scope.spawn(move || {
                for (slot, file) in out.iter_mut().zip(part.iter()) {
                    *slot = Some(parse_one(file));
                }
            }));
        }
        for handle in handles {
            // A panicking worker (an allocation failure inside serde) is a broken
            // build, never a silent success.
            if handle.join().is_err() {
                return Err(ArchiveError::Corrupt(
                    "manifest parse worker panicked".to_string(),
                ));
            }
        }
        Ok::<(), ArchiveError>(())
    })?;
    let mut parsed = Vec::with_capacity(files.len());
    for slot in slots {
        match slot {
            // Every index is filled by its worker; a hole is a logic error, and a
            // manifest set that silently loses a body would lose references.
            None => {
                return Err(ArchiveError::Corrupt(format!(
                    "manifest parse incomplete ({} of {} files)",
                    parsed.len(),
                    files.len()
                )))
            }
            Some(result) => parsed.push(result?),
        }
    }
    Ok(parsed)
}

fn parse_one(file: &ManifestFile) -> Result<Manifest, ArchiveError> {
    let raw = std::fs::read(&file.path).map_err(|e| crate::error::AppError::io(&file.path, e))?;
    let manifest: Manifest = serde_json::from_slice(&raw)
        .map_err(|e| ArchiveError::Corrupt(format!("manifest {}: {e}", file.name)))?;
    MANIFEST_PARSES.fetch_add(1, Ordering::Relaxed);
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Record-side index: the record references plus the capture-id → carrier-kind
/// map the record decision needs. Built on demand (see
/// [`ManifestSnapshot::check_record`]).
#[derive(Debug, Clone, Default)]
struct RecordIndex {
    records: RefTable,
    /// capture id -> whether the carrier stores an application-level compressed
    /// payload (`source.kind == "sqlite"`), last writer wins as in the scan.
    sqlite_captures: HashMap<String, bool>,
}

/// Immutable view of the committed manifest set of one archive.
///
/// Dereferences to the manifest slice, so `snapshot.iter()`, `snapshot.len()` and
/// `&snapshot[..]` behave like the `Vec<Manifest>` the callers used to get.
pub struct ManifestSnapshot {
    files: Vec<(String, FileStamp)>,
    manifests: Vec<Manifest>,
    /// object sha -> its committed references (probe order; the decision puts
    /// them back in manifest order, see `check_object`.)
    objects: RefTable,
    /// record sha -> its committed references; empty until a record access check
    /// needs it.
    records: OnceLock<Box<RecordIndex>>,
}

impl Clone for ManifestSnapshot {
    fn clone(&self) -> ManifestSnapshot {
        // `OnceLock` is cloned through its contents: a copy must carry whatever
        // the original has already built.
        let records = OnceLock::new();
        if let Some(index) = self.records.get() {
            let _ = records.set(index.clone());
        }
        ManifestSnapshot {
            files: self.files.clone(),
            manifests: self.manifests.clone(),
            objects: self.objects.clone(),
            records,
        }
    }
}

impl Deref for ManifestSnapshot {
    type Target = [Manifest];

    fn deref(&self) -> &[Manifest] {
        &self.manifests
    }
}

impl ManifestSnapshot {
    /// Parse the given manifest files (oldest first) into a snapshot.
    pub(crate) fn parse(files: &[ManifestFile]) -> Result<ManifestSnapshot, ArchiveError> {
        let mut snapshot = ManifestSnapshot {
            files: Vec::with_capacity(files.len()),
            manifests: Vec::with_capacity(files.len()),
            objects: RefTable::new(),
            records: OnceLock::new(),
        };
        snapshot.push(files)?;
        BUILDS.fetch_add(1, Ordering::Relaxed);
        Ok(snapshot)
    }

    /// Extend an existing snapshot with manifest files appended after it.
    pub(crate) fn extend(&mut self, files: &[ManifestFile]) -> Result<(), ArchiveError> {
        self.push(files)?;
        note_extension();
        Ok(())
    }

    fn push(&mut self, files: &[ManifestFile]) -> Result<(), ArchiveError> {
        // Parse first: a corrupt manifest leaves the snapshot untouched instead of
        // half-extended.
        let parsed = parse_manifests(files)?;
        let mut probes = 0u64;
        let mut object_refs = 0u64;
        let mut record_refs = 0u64;
        for (file, manifest) in files.iter().zip(parsed) {
            let manifest_index = u32::try_from(self.manifests.len())
                .map_err(|_| ArchiveError::Corrupt("manifest set is too large".to_string()))?;
            self.files.push((file.name.clone(), file.stamp));
            self.objects.reserve(manifest.objects.len());
            for (entry, object) in manifest.objects.iter().enumerate() {
                probes += self.objects.insert(
                    &object.sha256,
                    EntryRef {
                        manifest: manifest_index,
                        entry: u32::try_from(entry).unwrap_or(u32::MAX),
                    },
                );
                object_refs += 1;
            }
            // A record index that was already built must stay complete: extend it
            // with this manifest's records as well.
            if let Some(index) = self.records.get_mut() {
                index.records.reserve(manifest.records.len());
                for (entry, record) in manifest.records.iter().enumerate() {
                    probes += index.records.insert(
                        &record.sha256,
                        EntryRef {
                            manifest: manifest_index,
                            entry: u32::try_from(entry).unwrap_or(u32::MAX),
                        },
                    );
                    record_refs += 1;
                }
                index.sqlite_captures.insert(
                    manifest.capture_id.clone(),
                    manifest.source.kind == "sqlite",
                );
            }
            self.manifests.push(manifest);
        }
        INSERT_PROBES.fetch_add(probes, Ordering::Relaxed);
        OBJECT_REFS.fetch_add(object_refs, Ordering::Relaxed);
        RECORD_REFS.fetch_add(record_refs, Ordering::Relaxed);
        crate::cost::counter("manifest_index.object_refs", object_refs);
        crate::cost::counter("manifest_index.record_refs", record_refs);
        Ok(())
    }

    /// The record index, built from the committed manifests on first use.
    fn record_index(&self) -> &RecordIndex {
        self.records.get_or_init(|| {
            RECORD_INDEX_BUILDS.fetch_add(1, Ordering::Relaxed);
            crate::cost::counter("manifest_index.record_builds", 1);
            let mut index = RecordIndex::default();
            let mut refs = 0u64;
            for (manifest_index, manifest) in self.manifests.iter().enumerate() {
                let manifest_index = u32::try_from(manifest_index).unwrap_or(u32::MAX);
                index.records.reserve(manifest.records.len());
                for (entry, record) in manifest.records.iter().enumerate() {
                    // The probe count of this lazy pass is reported through the
                    // same counter as the eager build.
                    INSERT_PROBES.fetch_add(
                        index.records.insert(
                            &record.sha256,
                            EntryRef {
                                manifest: manifest_index,
                                entry: u32::try_from(entry).unwrap_or(u32::MAX),
                            },
                        ),
                        Ordering::Relaxed,
                    );
                    refs += 1;
                }
                index.sqlite_captures.insert(
                    manifest.capture_id.clone(),
                    manifest.source.kind == "sqlite",
                );
            }
            RECORD_REFS.fetch_add(refs, Ordering::Relaxed);
            crate::cost::counter("manifest_index.record_refs", refs);
            Box::new(index)
        })
    }

    /// Manifest file names this snapshot was built from (ascending).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|(name, _)| name.as_str())
    }

    /// True when `files` is exactly the listing this snapshot was built from
    /// (same names, same sizes, same modification times).
    pub(crate) fn matches(&self, files: &[ManifestFile]) -> bool {
        self.files.len() == files.len()
            && self
                .files
                .iter()
                .zip(files.iter())
                .all(|((name, stamp), file)| name == &file.name && *stamp == file.stamp)
    }

    /// The manifest files appended after this snapshot, or `None` when the
    /// listing is not a pure append.
    pub(crate) fn appended_after<'a>(
        &self,
        files: &'a [ManifestFile],
    ) -> Option<&'a [ManifestFile]> {
        if files.len() < self.files.len() {
            return None;
        }
        let append_only = self
            .files
            .iter()
            .zip(files.iter())
            .all(|((name, stamp), file)| name == &file.name && *stamp == file.stamp);
        if !append_only {
            return None;
        }
        Some(&files[self.files.len()..])
    }

    /// The manifest slice (same as `&*self`).
    pub fn as_slice(&self) -> &[Manifest] {
        &self.manifests
    }

    /// Access decision for one object sha: allowed iff `domain` appears in the
    /// access tags of EVERY committed manifest reference to it (strictest
    /// intersection). Unknown or unmapped (empty tags) references deny.
    ///
    /// The error variants and their order are the ones the per-read scan
    /// produced, so a denial before and after this index is indistinguishable.
    pub fn check_object(&self, sha256: &str, domain: &str) -> Result<(), ArchiveError> {
        if domain.is_empty() {
            return Err(ArchiveError::AccessDenied(
                "empty access domain (fail-closed)".to_string(),
            ));
        }
        let references = self.object_refs(sha256);
        if references.is_empty() {
            return Err(ArchiveError::AccessDenied(format!(
                "object {sha256} has no committed reference (orphan or unknown; fail-closed)"
            )));
        }
        for reference in references {
            let manifest = &self.manifests[reference.manifest as usize];
            let object = &manifest.objects[reference.entry as usize];
            if object.access.is_empty() {
                return Err(ArchiveError::AccessDenied(format!(
                    "object {sha256} has a reference with no access mapping (fail-closed)"
                )));
            }
            if !object.access.iter().any(|d| d == domain) {
                return Err(ArchiveError::AccessDenied(format!(
                    "object {sha256} is not visible to domain {domain:?} in capture {}",
                    manifest.capture_id
                )));
            }
        }
        Ok(())
    }

    /// Every committed reference to one object sha, in manifest order (the order
    /// the per-read scan used). A table candidate of another sha that shares the
    /// hash is not a reference and is dropped here.
    fn object_refs(&self, sha256: &str) -> Vec<EntryRef> {
        let mut references: Vec<EntryRef> = self
            .objects
            .candidates(sha256)
            .filter(|reference| {
                let manifest = &self.manifests[reference.manifest as usize];
                manifest.objects[reference.entry as usize].sha256 == sha256
            })
            .collect();
        references.sort_unstable();
        references
    }

    /// Access decision for one record; `Ok(true)` when the record's carrier is an
    /// application-level compressed SQLite batch whose manifest offsets address
    /// the decompressed payload.
    pub fn check_record(
        &self,
        record_sha: &str,
        rec: &RecordRef,
        domain: &str,
    ) -> Result<bool, ArchiveError> {
        if domain.is_empty() {
            return Err(ArchiveError::AccessDenied(
                "empty access domain (fail-closed)".to_string(),
            ));
        }
        let index = self.record_index();
        let batch_compressed = index
            .sqlite_captures
            .get(&rec.capture_id)
            .copied()
            .unwrap_or(false);
        let references = self.record_refs(record_sha);
        if references.is_empty() {
            return Err(ArchiveError::AccessDenied(format!(
                "record {record_sha} has no committed reference (fail-closed)"
            )));
        }
        for reference in references {
            let manifest = &self.manifests[reference.manifest as usize];
            let record = &manifest.records[reference.entry as usize];
            if record.access.is_empty() {
                return Err(ArchiveError::AccessDenied(format!(
                    "record {record_sha} has a reference with no access mapping (fail-closed)"
                )));
            }
            if !record.access.iter().any(|d| d == domain) {
                return Err(ArchiveError::AccessDenied(format!(
                    "record {record_sha} is not visible to domain {domain:?} in capture {}",
                    manifest.capture_id
                )));
            }
        }
        Ok(batch_compressed)
    }

    /// Every committed reference to one record sha, in manifest order.
    fn record_refs(&self, record_sha: &str) -> Vec<EntryRef> {
        let mut references: Vec<EntryRef> = self
            .record_index()
            .records
            .candidates(record_sha)
            .filter(|reference| {
                let manifest = &self.manifests[reference.manifest as usize];
                manifest.records[reference.entry as usize].sha256 == record_sha
            })
            .collect();
        references.sort_unstable();
        references
    }

    /// The earliest committed reference to `record_sha` in the shape the record
    /// ledger stores it: `(object, offset, len, capture_id)` of the **first**
    /// manifest that carries the record.
    ///
    /// That is the ledger's first-writer mapping. A record re-captured after a
    /// crashed round gets a new capture id (and a new carrier object); recording
    /// the new mapping would make the ledger differ from "replay the manifests
    /// in capture order, first writer wins". Resolving the earliest committed
    /// mapping keeps the two equal in every crash window. `None` when no
    /// committed manifest references the record (nothing to resolve).
    pub fn earliest_record_ref(&self, record_sha: &str) -> Option<RecordRef> {
        let reference = self.record_refs(record_sha).into_iter().next()?;
        let manifest = &self.manifests[reference.manifest as usize];
        let record = manifest.records.get(reference.entry as usize)?;
        Some(RecordRef {
            object: record.object.clone(),
            offset: record.offset,
            len: record.len,
            capture_id: manifest.capture_id.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Unit tests: the probe table and the lazy record index
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(manifest: u32, entry: u32) -> EntryRef {
        EntryRef { manifest, entry }
    }

    /// One synthetic manifest: `sha`s are `format!("{prefix}{index}")` padded to
    /// 64 hex characters.
    fn manifest(
        capture_id: &str,
        kind: &str,
        objects: &[String],
        records: &[String],
        access: &[&str],
    ) -> Manifest {
        serde_json::from_value(json!({
            "schema": "agent-memory/manifest@1",
            "capture_id": capture_id,
            "captured_at_ms": 1,
            "device_id": "device",
            "provider": "claude_code",
            "source": {
                "kind": kind,
                "path": "/tmp/source.jsonl",
                "fingerprint": {
                    "mtime_ms": 0,
                    "len": 0,
                    "head_sha256": "",
                    "tail_sha256": ""
                },
                "source_state_at_capture": ""
            },
            "objects": objects
                .iter()
                .map(|sha| json!({"sha256": sha, "bytes": 1, "role": "whole_file", "access": access}))
                .collect::<Vec<_>>(),
            "records": records
                .iter()
                .enumerate()
                .map(|(index, sha)| json!({
                    "seq": index,
                    "offset": index,
                    "len": 1,
                    "sha256": sha,
                    "object": objects.first().cloned().unwrap_or_default(),
                    "access": access
                }))
                .collect::<Vec<_>>()
        }))
        .expect("manifest fixture")
    }

    fn snapshot_from(manifests: Vec<Manifest>) -> ManifestSnapshot {
        let mut objects = RefTable::new();
        for (manifest_index, manifest) in manifests.iter().enumerate() {
            for (entry_index, object) in manifest.objects.iter().enumerate() {
                objects.insert(
                    &object.sha256,
                    entry(manifest_index as u32, entry_index as u32),
                );
            }
        }
        ManifestSnapshot {
            files: Vec::new(),
            manifests,
            objects,
            records: OnceLock::new(),
        }
    }

    /// Every reference of a sha is found (whatever order the table probes in), and
    /// a sha that was never inserted yields nothing.
    #[test]
    fn candidates_find_every_reference() {
        let mut table = RefTable::new();
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let mut inserted = Vec::new();
        for round in 0..40u32 {
            table.insert(&first, entry(round, round + 1));
            table.insert(&second, entry(round, round + 2));
            inserted.push(entry(round, round + 1));
        }
        let mut found: Vec<EntryRef> = table.candidates(&first).collect();
        found.sort_unstable();
        inserted.sort_unstable();
        assert_eq!(found, inserted);
        assert_eq!(table.candidates(&"c".repeat(64)).count(), 0);
    }

    /// A grown table still finds every key, and the load factor keeps the insert
    /// probes linear: a log-time search over 20 000 keys would probe ~15 slots per
    /// insert, the table probes a handful.
    #[test]
    fn growth_keeps_every_reference() {
        let mut table = RefTable::new();
        let mut probes = 0;
        for index in 0..20_000u32 {
            probes += table.insert(&format!("{index:064x}"), entry(index, index));
        }
        assert_eq!(table.len(), 20_000);
        for index in 0..20_000u32 {
            let found: Vec<EntryRef> = table.candidates(&format!("{index:064x}")).collect();
            assert_eq!(found, vec![entry(index, index)]);
        }
        assert!(probes < 20_000 * 4, "probes={probes}");
        assert!(table.slots.len() >= 20_000 * 2);
    }

    /// Manifest order is restored by the caller, not by the table: the decision
    /// order survives the re-probe a resize performs (which can move a wrapped
    /// probe run to the front of the slot array).
    #[test]
    fn manifest_order_survives_table_growth() {
        let shared = "9".repeat(64);
        let mut manifests = Vec::new();
        for index in 0..80u32 {
            manifests.push(manifest(
                &format!("cap-{index:03}"),
                "claude_code",
                std::slice::from_ref(&shared),
                &[],
                &["default"],
            ));
        }
        let snapshot = snapshot_from(manifests);
        let references = snapshot.object_refs(&shared);
        assert_eq!(references.len(), 80);
        for (index, reference) in references.iter().enumerate() {
            assert_eq!(*reference, self::entry(index as u32, 0));
        }
    }

    /// A slot whose hash matches the request but whose manifest entry holds a
    /// different sha is a collision, not a reference: it is filtered by the sha
    /// comparison and never admits or denies an object on its own.
    #[test]
    fn a_hash_collision_never_changes_the_decision() {
        let first = "1".repeat(64);
        let second = "2".repeat(64);
        let absent = "3".repeat(64);
        let mut snapshot = snapshot_from(vec![
            manifest("cap-one", "claude_code", std::slice::from_ref(&first), &[], &["default"]),
            manifest("cap-two", "claude_code", std::slice::from_ref(&second), &[], &["default"]),
        ]);
        // Force the collision shape: these slots hash like `absent`, but point at
        // the first manifest's object (a different sha).
        snapshot
            .objects
            .insert_hashed(sha_hash(&absent), entry(0, 0));
        snapshot
            .objects
            .insert_hashed(sha_hash(&absent), entry(1, 0));
        // The real references still decide.
        snapshot.check_object(&first, "default").expect("first");
        snapshot.check_object(&second, "default").expect("second");
        // An unresolvable sha is denied with the orphan message even though its
        // probe run is not empty.
        match snapshot.check_object(&absent, "default") {
            Err(ArchiveError::AccessDenied(message)) => {
                assert!(message.contains("no committed reference"), "{message}")
            }
            other => panic!("expected a denial for an absent sha, got {other:?}"),
        }
    }

    /// The record index is built on the first record check and is then reused; the
    /// decision (including the empty-tag denial) is the one of the per-read scan.
    #[test]
    fn the_record_index_is_built_on_first_check() {
        let object = "4".repeat(64);
        let first = "5".repeat(64);
        let second = "6".repeat(64);
        let manifests = vec![
            manifest(
                "cap-record-one",
                "claude_code",
                std::slice::from_ref(&object),
                std::slice::from_ref(&first),
                &["default"],
            ),
            manifest(
                "cap-record-two",
                "claude_code",
                std::slice::from_ref(&object),
                std::slice::from_ref(&second),
                &["default"],
            ),
        ];
        let snapshot = snapshot_from(manifests.clone());
        let reference = |manifest: &Manifest| RecordRef {
            object: object.clone(),
            offset: 0,
            len: 1,
            capture_id: manifest.capture_id.clone(),
        };
        let rec = reference(&manifests[1]);
        assert!(!snapshot
            .check_record(&second, &rec, "default")
            .expect("second record is readable"));
        assert!(snapshot
            .check_record(&first, &reference(&manifests[0]), "other")
            .is_err());
        match snapshot.check_record(&"7".repeat(64), &rec, "default") {
            Err(ArchiveError::AccessDenied(message)) => {
                assert!(message.contains("no committed reference"), "{message}")
            }
            other => panic!("expected a denial for an unknown record, got {other:?}"),
        }
        // A record whose committed reference has no access tags denies, even when
        // another capture of the same record allows the domain.
        let mut unmapped = manifests[1].clone();
        unmapped.capture_id = "cap-record-unmapped".to_string();
        unmapped.records[0].access.clear();
        let mut manifests = manifests;
        manifests.push(unmapped);
        let snapshot = snapshot_from(manifests);
        match snapshot.check_record(&second, &rec, "default") {
            Err(ArchiveError::AccessDenied(message)) => {
                assert!(message.contains("no access mapping"), "{message}")
            }
            other => panic!("expected a denial for an empty access mapping, got {other:?}"),
        }
    }
}
