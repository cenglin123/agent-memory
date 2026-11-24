//! Local evidence archive: content-addressed objects, capture manifests,
//! per-source watermarks, single-writer lock, fail-closed reference readback.
//!
//! Layout under the runtime root:
//! ```text
//! <root>/archive/objects/<sha[0..2]>/<sha[2..4]>/<sha256>       object, plain encoding
//! <root>/archive/objects/<sha[0..2]>/<sha[2..4]>/<sha256>.zst   object, zstd encoding
//! <root>/archive/manifests/<seq>-<capture_id>.json          committed manifests
//! <root>/archive/staging/                                    in-flight object writes
//! <root>/state/sources.json    per-source watermark + capture counters
//! <root>/state/records.json    record-sha -> object span dedupe index (canonical)
//! <root>/state/records.jsonl   journal of that same ledger, appended per round
//! <root>/state/schema.json     archive schema + derived-layer versions
//! <root>/state/device_id       install-stable device id
//! <root>/state/sqlite/<hash32>.obs.zst  P3 last SQLite observation (collector state)
//! <root>/state/sqlite/<hash32>.gate-verify.json  verification gate bookkeeping
//! <root>/state/scrub.json               P5 scrub ledger (damage bookkeeping)
//! <root>/derived/state.json             P5 derived checkpoint index
//! <root>/derived/generations/g<N>/...    P5 per-source derived artifacts
//! <root>/locks/capture.lock             single-writer capture/scrub lock
//! <root>/locks/derived.lock             single-writer derived replay lock
//! <root>/locks/records-compact.lock     single compactor for the record ledger
//! <root>/locks/records-tail.lock        one journal writer at a time
//! <root>/logs/                 bounded run logs (P1: created, unused)
//! ```
//!
//! Manifests record capture facts only (hashes, sizes, times, device, in-source
//! locator, source state at capture, reference-level access tags). Logical
//! session ownership and parser versions belong to the derived layer and are
//! never written into manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::atomic_write;
use crate::error::AppError;
use crate::model::ContentRef;

mod snapshot;

use snapshot::{note_copy, note_list, FileStamp, ManifestFile};
pub use snapshot::{snapshot_stats, ManifestSnapshot, SnapshotStats};

mod records;
pub use records::{
    RecordJournalLine, RECORDS_CANONICAL_FILENAME, RECORDS_CANONICAL_PATH,
    RECORDS_TAIL_COMPACT_BYTES, RECORDS_TAIL_COMPACT_MAX_AGE_MS, RECORDS_TAIL_FILENAME,
    RECORDS_TAIL_PATH,
};

pub const MANIFEST_SCHEMA: &str = "agent-memory/manifest@1";
pub const STATE_SCHEMA: &str = "agent-memory/state@1";
pub const RECORDS_SCHEMA: &str = "agent-memory/records@1";
pub const ARCHIVE_SCHEMA_VERSION: u32 = 1;
pub const DERIVED_MODEL_VERSION: u32 = 1;
/// Chunk size for streamed object writes/verification (P4). Memory use for an
/// attachment is bounded by this value, not by the attachment size.
pub const OBJECT_CHUNK_BYTES: usize = 64 * 1024;

/// Manifest files read and parsed by [`Archive::manifests`] in this process.
/// Test instrumentation for the one-parse-per-run guarantee: the number must
/// follow the manifest count, never `carriers x manifests`.
static MANIFEST_PARSES: AtomicU64 = AtomicU64::new(0);

/// Process-wide count of parsed manifest files (see [`MANIFEST_PARSES`]).
pub fn manifest_parse_count() -> u64 {
    MANIFEST_PARSES.load(Ordering::Relaxed)
}

// -- object storage encoding (stage-2 S2.5) --------------------------------

/// zstd compression level for object storage encoding. A central constant so
/// every writer (capture and `compact`) produces the same encoding.
pub const ZSTD_COMPRESSION_LEVEL: i32 = 3;
/// Filename suffix that marks the zstd storage encoding. The encoding is
/// *explicitly decidable from the file name* — never from content sniffing
/// (a plain-encoded object may legitimately start with the zstd magic, e.g. a
/// whole-file capture of a `.zst` source).
pub const ZSTD_OBJECT_SUFFIX: &str = ".zst";
/// An object is stored compressed only when the encoded form is *worth it*:
/// strictly smaller than this percentage of the raw size. Already-compressed
/// content (`.jsonl.zst` whole files, SQLite zstd chunks/deltas) fails the
/// test and stays plain, so the threshold is also the "do not double-compress"
/// guard for content whose nature is not declared by the writer.
pub const ZSTD_WORTH_PERCENT: u64 = 95;

/// Storage encoding of one object file. The object id is always the sha256 of
/// the *decoded* (original) bytes; the encoding only decides how those bytes
/// are laid on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectEncoding {
    /// Stored verbatim: `<sha256>`.
    Plain,
    /// Stored as one zstd frame (level [`ZSTD_COMPRESSION_LEVEL`]):
    /// `<sha256>.zst`.
    Zstd,
}

impl ObjectEncoding {
    pub fn suffix(self) -> &'static str {
        match self {
            ObjectEncoding::Plain => "",
            ObjectEncoding::Zstd => ZSTD_OBJECT_SUFFIX,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ObjectEncoding::Plain => "plain",
            ObjectEncoding::Zstd => "zstd",
        }
    }
}

/// Writer-side hint for [`Archive::put_object_encoded`]. Sources whose content
/// is *known* to be already compressed declare [`StorageHint::Raw`] so no
/// compression attempt is made at all; compressible sources declare
/// [`StorageHint::Compressible`] and the worth threshold decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageHint {
    /// Store verbatim (already-compressed sources: `.jsonl.zst` whole files,
    /// SQLite baseline chunks / delta batches — their object bytes are already
    /// a zstd payload).
    Raw,
    /// Try zstd level 3; keep the plain encoding when the compressed form is
    /// not smaller than [`ZSTD_WORTH_PERCENT`] percent of the raw size.
    Compressible,
}

/// Per-encoding storage statistics (object count, original bytes, stored
/// bytes). `original_bytes` for the zstd group is the decoded content size;
/// for the plain group it equals `stored_bytes` by definition. While a
/// `compact` run sits in its crash window both encodings of one sha may exist;
/// each file is then counted in its own group (transient double counting that
/// the next `compact` converges away).
#[derive(Debug, Clone, Copy, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StorageStats {
    pub plain_objects: u64,
    pub plain_original_bytes: u64,
    pub plain_stored_bytes: u64,
    pub zstd_objects: u64,
    pub zstd_original_bytes: u64,
    pub zstd_stored_bytes: u64,
    /// Unreferenced `.zst` objects whose length could not be decoded (external
    /// damage or a hand-placed file). They stay orphans in `reconcile`; the
    /// statistic never fails the capture/status path because of them.
    pub unreadable_objects: u64,
}

impl StorageStats {
    pub fn total_objects(&self) -> u64 {
        self.plain_objects + self.zstd_objects
    }

    pub fn total_original_bytes(&self) -> u64 {
        self.plain_original_bytes + self.zstd_original_bytes
    }

    pub fn total_stored_bytes(&self) -> u64 {
        self.plain_stored_bytes + self.zstd_stored_bytes
    }
}

/// One object file found on disk (explicit encoding, metadata only).
#[derive(Debug, Clone)]
pub struct ObjectFileEntry {
    pub sha256: String,
    pub encoding: ObjectEncoding,
    pub path: PathBuf,
    pub stored_bytes: u64,
}

/// P4: manifest source kind for an attachment/content-reference capture. Such a
/// manifest is an attachment evidence unit, not a watermark for the physical
/// session source, so `Archive::source_captures` deliberately excludes it.
pub const ATTACHMENT_SOURCE_KIND: &str = "attachment";

/// P4: outcome of one attachment/content-reference capture. Every value is a
/// capture fact recorded in the manifest and projected into the coverage report;
/// none of them is a silent skip.
pub const ATTACHMENT_STATUS_ARCHIVED: &str = "archived";
pub const ATTACHMENT_STATUS_MISSING: &str = "missing";
pub const ATTACHMENT_STATUS_PATH_TRAVERSAL: &str = "path_traversal";
pub const ATTACHMENT_STATUS_OUT_OF_BOUNDS: &str = "out_of_bounds";
pub const ATTACHMENT_STATUS_UNAUTHORIZED: &str = "unauthorized";
pub const ATTACHMENT_STATUS_OVER_LIMIT: &str = "over_limit";
pub const ATTACHMENT_STATUS_EXTERNAL_REFERENCE: &str = "external_reference";
pub const ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE: &str = "unsupported_reference";
pub const ATTACHMENT_STATUS_READ_FAILED: &str = "read_failed";
pub const ATTACHMENT_STATUS_SOURCE_MISSING: &str = "source_missing";
pub const ATTACHMENT_STATUS_RECORD_MISMATCH: &str = "record_mismatch";

/// Per-run attachment outcome counters. `refs` counts every requested reference;
/// `archived` counts the ones whose bytes are in the object store (including
/// content-addressed dedupe hits, which are also counted in `deduplicated`).
/// `refs - archived` is therefore the number of explicitly rejected references.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentCounts {
    pub refs: u64,
    pub archived: u64,
    pub deduplicated: u64,
    pub missing: u64,
    pub path_traversal: u64,
    pub out_of_bounds: u64,
    pub unauthorized: u64,
    pub over_limit: u64,
    pub external_reference: u64,
    pub unsupported_reference: u64,
    pub read_failed: u64,
    pub source_missing: u64,
    pub record_mismatch: u64,
}

impl AttachmentCounts {
    /// Count one reference by its explicit status.
    pub fn record(&mut self, status: &str) {
        self.refs = self.refs.saturating_add(1);
        match status {
            ATTACHMENT_STATUS_ARCHIVED => self.archived = self.archived.saturating_add(1),
            ATTACHMENT_STATUS_MISSING => self.missing = self.missing.saturating_add(1),
            ATTACHMENT_STATUS_PATH_TRAVERSAL => {
                self.path_traversal = self.path_traversal.saturating_add(1)
            }
            ATTACHMENT_STATUS_OUT_OF_BOUNDS => self.out_of_bounds = self.out_of_bounds.saturating_add(1),
            ATTACHMENT_STATUS_UNAUTHORIZED => self.unauthorized = self.unauthorized.saturating_add(1),
            ATTACHMENT_STATUS_OVER_LIMIT => self.over_limit = self.over_limit.saturating_add(1),
            ATTACHMENT_STATUS_EXTERNAL_REFERENCE => {
                self.external_reference = self.external_reference.saturating_add(1)
            }
            ATTACHMENT_STATUS_UNSUPPORTED_REFERENCE => {
                self.unsupported_reference = self.unsupported_reference.saturating_add(1)
            }
            ATTACHMENT_STATUS_READ_FAILED => self.read_failed = self.read_failed.saturating_add(1),
            ATTACHMENT_STATUS_SOURCE_MISSING => {
                self.source_missing = self.source_missing.saturating_add(1)
            }
            ATTACHMENT_STATUS_RECORD_MISMATCH => {
                self.record_mismatch = self.record_mismatch.saturating_add(1)
            }
            _ => {}
        }
    }

    /// References that were not archived (every explicit rejection).
    pub fn rejected(&self) -> u64 {
        self.refs.saturating_sub(self.archived)
    }

    pub fn add(&mut self, other: &AttachmentCounts) {
        self.refs = self.refs.saturating_add(other.refs);
        self.archived = self.archived.saturating_add(other.archived);
        self.deduplicated = self.deduplicated.saturating_add(other.deduplicated);
        self.missing = self.missing.saturating_add(other.missing);
        self.path_traversal = self.path_traversal.saturating_add(other.path_traversal);
        self.out_of_bounds = self.out_of_bounds.saturating_add(other.out_of_bounds);
        self.unauthorized = self.unauthorized.saturating_add(other.unauthorized);
        self.over_limit = self.over_limit.saturating_add(other.over_limit);
        self.external_reference = self
            .external_reference
            .saturating_add(other.external_reference);
        self.unsupported_reference = self
            .unsupported_reference
            .saturating_add(other.unsupported_reference);
        self.read_failed = self.read_failed.saturating_add(other.read_failed);
        self.source_missing = self.source_missing.saturating_add(other.source_missing);
        self.record_mismatch = self.record_mismatch.saturating_add(other.record_mismatch);
    }
}

// ---------------------------------------------------------------------------
// Types (manifest / state)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub mtime_ms: i64,
    pub len: u64,
    pub head_sha256: String,
    pub tail_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSource {
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compressed: bool,
    pub fingerprint: SourceFingerprint,
    pub source_state_at_capture: String,
    /// P2: why this capture happened — `initial` | `append` | `forced_recapture`.
    /// A capture fact (what the collector did), never a parser judgement.
    #[serde(default = "default_capture_reason")]
    pub capture_reason: String,
    /// P2: 1-based capture generation for this physical source path. A forced
    /// recapture after a non-append change bumps it; appends keep it.
    #[serde(default)]
    pub generation: u64,
    /// P2: first source byte covered by this capture's object range.
    #[serde(default)]
    pub range_start: u64,
    /// DSH only: the carrier relative path inside the source root
    /// (`<project>/<session-dir>/<generation file>`). The source `path` stays
    /// the session directory, so a generation switch keeps one identity and
    /// supersedes the previous capture generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    /// P3: SQLite evidence metadata (baseline snapshot or raw row delta). Absent
    /// for non-SQLite sources. A capture fact only: it records what was read and
    /// when, never a transaction boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite: Option<ManifestSqlite>,
}

/// P3 (stage-2 S1): one chunk of a chunked baseline export.
///
/// A baseline is no longer one database image: it is a list of per-table
/// content-addressed chunk objects (role `sqlite_baseline_chunk`), each a
/// zstd-compressed JSONL export of whitelisted rows in primary-key order.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ManifestSqliteChunk {
    /// Whitelisted table this chunk belongs to (a chunk never spans tables).
    pub table: String,
    /// Content address of the compressed chunk object.
    pub sha256: String,
    /// Uncompressed chunk byte length.
    pub bytes: u64,
    /// Row lines in this chunk.
    pub rows: u64,
    /// sha256 of the uncompressed chunk bytes (readback verification).
    pub raw_sha256: String,
    /// Smallest / largest row key in this chunk (lexicographic over the
    /// canonical key strings). `before` lookups decompress only chunks whose
    /// [min, max] range contains the target key, so a delta capture reads
    /// evidence proportional to what changed, not to the baseline size.
    #[serde(default)]
    pub min_key: String,
    #[serde(default)]
    pub max_key: String,
}

/// P3: capture facts for one SQLite evidence unit (baseline or delta batch).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ManifestSqlite {
    /// S4.5: capture profile the unit was taken under (`opencode`, ...). Empty
    /// on evidence written before profiles existed; such evidence is read as
    /// the OpenCode profile.
    #[serde(default)]
    pub profile_id: String,
    /// Monotonic capture (evidence) sequence for this physical source path.
    pub capture_seq: u64,
    /// 1-based baseline generation this unit belongs to.
    pub baseline_generation: u64,
    /// Fingerprint of the observed whitelist schema at this observation.
    pub schema_fingerprint: String,
    /// Why a baseline was taken (`initial`, `schema_change`,
    /// `unknown_table_or_column`, `delta_chain_threshold`, `filter_changed`,
    /// `rebaseline_requested`, ...). Empty for delta batches.
    #[serde(default)]
    pub snapshot_reason: String,
    /// Observation timestamp of this unit.
    pub observed_at_ms: i64,
    /// Gap since the previous observation of this source (blind window), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_interval_ms: Option<i64>,
    /// Rows observed in this unit (baseline: whole export; delta: changed rows).
    #[serde(default)]
    pub rows_discovered: u64,
    #[serde(default)]
    pub rows_inserted: u64,
    #[serde(default)]
    pub rows_updated: u64,
    #[serde(default)]
    pub rows_deleted: u64,
    /// Delta chain length after this unit (0 for a baseline).
    #[serde(default)]
    pub delta_chain_length: u64,
    /// Format tag of the baseline chunk export (`agent-memory/sqlite-baseline@2`).
    /// Empty for delta batches.
    #[serde(default)]
    pub baseline_schema: String,
    /// Digest over the ordered chunk list (baselines only): two exports with the
    /// same digest archived exactly the same whitelisted rows.
    #[serde(default)]
    pub baseline_digest: String,
    /// Total uncompressed baseline export size in bytes (baselines only).
    #[serde(default)]
    pub baseline_bytes: u64,
    /// Ordered chunk list (baselines only).
    #[serde(default)]
    pub baseline_chunks: Vec<ManifestSqliteChunk>,
    /// Declared column order per archived table (baselines only).
    #[serde(default)]
    pub baseline_columns: BTreeMap<String, Vec<String>>,
    /// `CREATE TABLE` statement per archived table (baselines only; replay
    /// materialization uses these instead of a snapshot database file).
    #[serde(default)]
    pub baseline_ddl: BTreeMap<String, String>,
    /// Per-table watermark `(value, primary key)` at export time (baselines
    /// only); lets a crash-adopted baseline resume watermark deltas.
    #[serde(default)]
    pub baseline_watermarks: BTreeMap<String, (i64, String)>,
    /// Delta batches only: per-table `(min key, max key)` over this batch's
    /// lines. `before` lookups decompress only batches whose range for a table
    /// contains the target key, instead of the whole delta chain.
    #[serde(default)]
    pub delta_table_ranges: BTreeMap<String, (String, String)>,
    /// True when this delta batch is a `--verify` correction, not a periodic
    /// observation delta.
    #[serde(default)]
    pub verify_correction: bool,
    /// Format tag of the delta line format (`agent-memory/sqlite-delta@2`).
    /// Empty for baselines.
    #[serde(default)]
    pub delta_schema: String,
}

fn default_capture_reason() -> String {
    "initial".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestObject {
    pub sha256: String,
    pub bytes: u64,
    pub role: String,
    /// Reference-level access tags frozen at capture time. An empty list is a
    /// missing permission mapping: readback must fail closed.
    pub access: Vec<String>,
    /// P2: source byte offset this object starts at (None for legacy manifests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<u64>,
    #[serde(default)]
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRecord {
    pub seq: u64,
    /// Offset of the record bytes inside `object` (readback locator).
    pub offset: u64,
    pub len: u64,
    pub sha256: String,
    /// Object holding the referenced bytes (decoded object for compressed sources).
    pub object: String,
    #[serde(default)]
    pub access: Vec<String>,
    /// P2: byte offset of this record in the *source file* at capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<u64>,
    /// P2: 1-based physical line number of this record in the source file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_line: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub capture_id: String,
    pub captured_at_ms: i64,
    pub device_id: String,
    pub provider: String,
    pub source: ManifestSource,
    pub objects: Vec<ManifestObject>,
    pub records: Vec<ManifestRecord>,
    /// P4: attachment / complete-content reference mappings. Every entry is a
    /// capture fact: the original [`ContentRef`] (in-source locator), how the
    /// bytes were obtained, the resolved local path (misses and rejects keep the
    /// explicit status), the object content address once archived, the frozen
    /// access tags and the size limit in force. A missing or rejected reference
    /// remains in the manifest — it is never silently omitted.
    #[serde(default)]
    pub attachments: Vec<ManifestAttachment>,
}

/// P4: one ContentRef -> archive object mapping (or the explicit reason there is
/// none). Stored verbatim in the manifest so the mapping is readable years after
/// the source file is gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestAttachment {
    /// 0-based sequence inside this manifest.
    pub seq: u64,
    /// The original reference exactly as it was given to the collector.
    pub content_ref: ContentRef,
    /// `content` (bytes located by the reference) or `local_file` (a file named
    /// by the reference).
    pub target: String,
    /// How the bytes were obtained: `content`, `record`, `sqlite_text`,
    /// `sqlite_blob`, `file_json`, `inline_base64`, `file`.
    pub read_mode: String,
    /// Normalized absolute path for file-based material (never for in-source
    /// content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    /// Detected or declared media type (`application/octet-stream` when unknown).
    pub media_type: String,
    /// One of the `ATTACHMENT_STATUS_*` values.
    pub status: String,
    /// Content address of the archived bytes; `None` iff not archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_sha256: Option<String>,
    /// Bytes archived (0 for a rejected reference).
    pub bytes: u64,
    /// Bytes observed at the source (file size or located content length), even
    /// when the reference was rejected as over-limit.
    pub observed_bytes: u64,
    /// Reference-level access tags frozen at capture time. Empty = no mapping
    /// (fail-closed).
    pub access: Vec<String>,
    /// True when the object already existed (content-addressed dedupe).
    pub deduplicated: bool,
    /// Size limit applied to this reference.
    pub limit_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Watermark {
    /// Full source length observed at capture time (may exceed `offset` when the
    /// source ended with an incomplete trailing record).
    pub len: u64,
    /// sha256 of the object written by the last capture run.
    pub sha256: String,
    pub fingerprint: SourceFingerprint,
    pub capture_id: String,
    pub captured_at_ms: i64,
    /// P2: committed byte offset (end of the last complete record + its newline).
    /// Only complete, committed records move this; it never exceeds `len`.
    #[serde(default)]
    pub offset: u64,
    /// P2: number of complete physical lines consumed up to `offset`.
    #[serde(default)]
    pub line: u64,
    /// P2: number of source records (non-blank complete lines) committed.
    #[serde(default)]
    pub records: u64,
    /// P2: capture generation for this physical source path (1-based).
    #[serde(default)]
    pub generation: u64,
    /// P2: sha256 of the first `probe_len` bytes of the committed prefix, used
    /// as a cheap append-versus-rewrite signal. Empty for non-JSONL sources.
    #[serde(default)]
    pub head_probe: String,
    /// P2: sha256 of the `probe_len` bytes ending at `offset`.
    #[serde(default)]
    pub anchor_probe: String,
    /// P2: probe window actually used (`min(offset, 8192)`).
    #[serde(default)]
    pub probe_len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceState {
    pub provider: String,
    /// Current health: captured | skipped_unchanged | source_missing | failed |
    /// degraded. (Discontinuity classification lives in `change`, not here.)
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<Watermark>,
    #[serde(default)]
    pub discovered: u64,
    #[serde(default)]
    pub captured: u64,
    #[serde(default)]
    pub skipped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_parse_error: Option<String>,
    /// P2: cumulative error conditions observed for this source.
    #[serde(default)]
    pub errors: u64,
    /// P2: bytes committed into objects for this source (sum over captures).
    #[serde(default)]
    pub bytes: u64,
    /// P2: last change classification (`initial`|`append`|`skipped_unchanged`|
    /// `forced_recapture`|`degraded`).
    #[serde(default)]
    pub change: String,
    /// P2: current capture generation (mirrors the watermark).
    #[serde(default)]
    pub generation: u64,
    /// P2: source bytes after the last complete record that were held back.
    #[serde(default)]
    pub incomplete_tail_bytes: u64,
    /// P2: source length observed by the most recent run (including runs that
    /// committed nothing). `None` = not recorded (state written before P2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_len: Option<u64>,
    /// P2: anomalies from the last run (discontinuity reasons, malformed
    /// records). Fresh per run; empty means none observed.
    #[serde(default)]
    pub anomalies: Vec<String>,
    /// P3: SQLite evidence state (baseline generation, delta chain, observation
    /// watermark, known schema). Absent for non-SQLite sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite: Option<SqliteState>,
    /// P4: last attachment run health for this physical source. Only the last
    /// run is kept (historical attachment facts live in the manifests, which are
    /// the single authority for coverage counters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentState>,
    /// DSH only: the incremental checkpoint of the log stream. It is collector
    /// state, not evidence (the manifests and objects are the evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh: Option<DshState>,
}

/// DSH incremental checkpoint: which generation carrier was read, how many
/// complete logical lines were committed, and the digest of the last one.
///
/// A DSH carrier is an append-only frame stream, so the committed prefix is
/// identified by logical lines rather than a byte offset. The watermark's
/// `offset` is the logical byte end of that prefix, its `sha256` is the digest
/// of the whole committed prefix, and `line` mirrors `lines`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DshState {
    /// Carrier relative path (`<project>/<session-dir>/<generation file>`).
    pub rel_path: String,
    /// Complete logical lines committed (blank lines included).
    pub lines: u64,
    /// sha256 of the last committed logical line (sha256 of no bytes when
    /// `lines` is 0).
    pub last_line_sha256: String,
}

/// P4: collector health for attachment captures on one physical source.
///
/// This is a projection, not evidence: the committed manifests hold every
/// attachment entry (including rejected ones). State only answers "how did the
/// last run go", which is what `coverage.ok` needs — replaying an old failure as
/// a current anomaly would make health permanently red.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentState {
    pub last_run_at_ms: i64,
    /// `archived` | `partial` | `failed`
    pub last_status: String,
    /// Counts of the most recent attachment run for this source.
    pub last_run: AttachmentCounts,
    /// Size limit applied by the most recent run.
    pub last_limit_bytes: u64,
    /// Explicit per-reference anomalies of the most recent run
    /// (`<status>:<locator>: <detail>`); empty means none.
    pub anomalies: Vec<String>,
}

/// P3: per-source SQLite evidence state.
///
/// This is collector state, not evidence: it carries the delta-chain
/// bookkeeping, the current baseline identity (chunk digest, byte size) and the
/// session filter in force. The per-table watermark / primary-key-set state
/// that drives the next watermark diff lives in a separate compressed file
/// (`observation_file`). Evidence authority stays with the committed manifests
/// (baseline chunk objects + raw delta objects). Losing this state triggers a
/// new baseline instead of guessing a diff.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SqliteState {
    /// S4.5: capture profile in force (`opencode`, ...). Empty on state written
    /// before profiles existed (read as the OpenCode profile).
    #[serde(default)]
    pub profile_id: String,
    /// Evidence sequence of the last committed unit (baseline or delta).
    #[serde(default)]
    pub capture_seq: u64,
    /// Current baseline generation (1-based).
    #[serde(default)]
    pub baseline_generation: u64,
    /// `capture_seq` of the current baseline.
    #[serde(default)]
    pub baseline_capture_seq: u64,
    /// Content address (sha256) of the compressed baseline snapshot object.
    /// Superseded by the chunked baseline (`baseline_digest`); kept only as a
    /// serde default for state files written in between.
    #[serde(default)]
    pub baseline_object: String,
    /// Digest over the current baseline's ordered chunk list.
    #[serde(default)]
    pub baseline_digest: String,
    /// Total uncompressed baseline export size in bytes. The delta chain is
    /// re-baselined when its raw bytes exceed 50% of this.
    #[serde(default)]
    pub baseline_bytes: u64,
    /// Session time filter in force (`sessionSinceMs`); a change re-baselines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_since_ms: Option<i64>,
    /// When the last full row-hash verification ran (0 = never / baseline).
    #[serde(default)]
    pub last_verify_at_ms: i64,
    /// Rows corrected by verification, lifetime.
    #[serde(default)]
    pub verify_mismatches_total: u64,
    /// Non-whitelist tables: row counts only (facts; row content is never read).
    #[serde(default)]
    pub excluded_tables: BTreeMap<String, u64>,
    /// With a session filter in force: whitelist child tables without a
    /// `session_id` column were captured in full, out of the session scope (N3).
    #[serde(default)]
    pub unfiltered_tables: Vec<String>,
    /// Schema fingerprint of the current baseline generation.
    #[serde(default)]
    pub schema_fingerprint: String,
    /// Tables known to the current baseline generation.
    #[serde(default)]
    pub known_tables: Vec<String>,
    /// Columns known per table in the current baseline generation.
    #[serde(default)]
    pub known_columns: BTreeMap<String, Vec<String>>,
    /// Deltas committed since the current baseline.
    #[serde(default)]
    pub delta_chain_length: u64,
    /// Raw (uncompressed) delta bytes committed since the current baseline.
    #[serde(default)]
    pub delta_chain_bytes: u64,
    /// Observation timestamp that started the current delta chain.
    #[serde(default)]
    pub delta_chain_started_ms: i64,
    #[serde(default)]
    pub deltas_total: u64,
    #[serde(default)]
    pub baselines_total: u64,
    #[serde(default)]
    pub rows_inserted_total: u64,
    #[serde(default)]
    pub rows_updated_total: u64,
    #[serde(default)]
    pub rows_deleted_total: u64,
    /// Rows observed by the most recent observation.
    #[serde(default)]
    pub rows_discovered: u64,
    /// Distinct payloads observed identically in more than one table of the last
    /// observation (dedupe fact; both raw rows stay archived).
    #[serde(default)]
    pub duplicate_payload_rows: u64,
    /// Observation watermark: when this source was last read.
    #[serde(default)]
    pub last_observed_at_ms: i64,
    /// Observation before that, and the gap between the two (blind window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_observed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_interval_ms: Option<i64>,
    /// Size of the source `-wal` file at the last observation (reported only;
    /// never a snapshot trigger by itself).
    #[serde(default)]
    pub wal_bytes: u64,
    /// Last explicit health failure (`sqlite_locked`, `sqlite_unavailable`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_unavailable: Option<String>,
    /// Reason of the last baseline snapshot.
    #[serde(default)]
    pub last_snapshot_reason: String,
    /// Runtime-root-relative path of the compressed observation state file.
    #[serde(default)]
    pub observation_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcesFile {
    pub schema: String,
    pub sources: BTreeMap<String, SourceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordRef {
    pub object: String,
    pub offset: u64,
    pub len: u64,
    pub capture_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordsFile {
    pub schema: String,
    pub records: BTreeMap<String, RecordRef>,
}

/// How often this archive instance touched the two state ledgers. One round is
/// expected to read each ledger at most once and write it at most once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerStats {
    pub records_loads: u64,
    /// Append writes: one per flush that staged new records. Compactions are
    /// counted separately (`records_compactions`).
    pub records_flushes: u64,
    /// Bytes of the canonical ledger and the journal parsed by those loads.
    pub records_read_bytes: u64,
    /// Bytes appended to the journal by those flushes.
    pub records_written_bytes: u64,
    /// Journal appends performed by those flushes.
    pub records_appends: u64,
    /// Journal bytes written by those appends (same as `records_written_bytes`,
    /// kept as the explicit "append only" figure).
    pub records_append_bytes: u64,
    /// Compactions performed (canonical rewritten from the merged ledger).
    pub records_compactions: u64,
    /// Canonical bytes written by those compactions.
    pub records_compact_bytes: u64,
    /// Degraded flushes that wrote the staged entries into the canonical
    /// instead of appending them to the journal (the journal lock could not be
    /// taken; see `Archive::flush_records_inner`).
    pub records_promotions: u64,
    /// Journal bytes parsed by those loads.
    pub records_tail_bytes: u64,
    /// Unterminated last journal lines ignored by those loads (append crash
    /// window; the next append truncates them away).
    pub records_tail_incomplete_lines: u64,
    /// Journal lines skipped because their sha was already present (compaction
    /// crash window, re-captured records).
    pub records_tail_duplicate_lines: u64,
    /// Failed automatic compactions. The journal stays readable; the last error
    /// is available as `Archive::records_compact_error`.
    pub records_compact_errors: u64,
    /// Staged entries whose mapping was replaced by the earliest committed
    /// manifest reference (a re-capture after a crashed round; normally `0`).
    pub records_first_writer_resolved: u64,
    pub sources_loads: u64,
    pub sources_flushes: u64,
    pub sources_read_bytes: u64,
    pub sources_written_bytes: u64,
    /// True while a capture-all round stages ledger writes.
    pub deferred: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedVersion {
    pub parser_version: String,
    pub model_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaFile {
    pub schema_version: u32,
    pub derived: DerivedVersion,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileReport {
    pub objects_on_disk: u64,
    pub objects_referenced: u64,
    pub manifests: u64,
    pub orphan_objects: Vec<String>,
    pub missing_objects: Vec<String>,
    pub staging_residue: Vec<String>,
    /// S2.5: per-encoding storage statistics (plain vs zstd groups).
    pub storage: StorageStats,
    /// S2.5: shas present in *both* encodings at once (a `compact` crash
    /// window). Reads prefer the zstd file; the next `compact` converges these
    /// to a single encoding. Empty in a settled archive.
    pub dual_encoding: Vec<String>,
}

/// One capture of one physical source, projected from committed manifests.
/// Version history lives in the manifests (the capture facts); this is a
/// derived summary, never a second authority.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureSummary {
    pub capture_id: String,
    pub captured_at_ms: i64,
    pub capture_reason: String,
    pub generation: u64,
    pub range_start: u64,
    pub records: u64,
    pub bytes: u64,
}

/// P4: one ContentRef -> object mapping projected from the committed manifests.
/// `attachment_id` is the stable readback handle (`<captureId>#<seq>`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentSummary {
    pub attachment_id: String,
    pub capture_id: String,
    pub captured_at_ms: i64,
    pub provider: String,
    pub source: String,
    pub content_ref: ContentRef,
    pub target: String,
    pub read_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    pub media_type: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub bytes: u64,
    pub observed_bytes: u64,
    pub access: Vec<String>,
    pub deduplicated: bool,
    pub limit_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True only when the entry is archived and its object is still on disk.
    pub readable: bool,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ArchiveError {
    Io(AppError),
    NotInitialized(PathBuf),
    LockHeld(PathBuf),
    /// Fail-closed: object/record unknown, access mapping missing, or the
    /// requesting domain is outside the strictest intersection of all refs.
    AccessDenied(String),
    NotFound(String),
    Integrity(String),
    Corrupt(String),
    /// P4: a streamed object exceeded its size limit; nothing was committed.
    LimitExceeded(u64),
    /// P3: the optional embedding helper could not produce this generation's
    /// vectors. The archive is untouched and lexical replay is still possible.
    Embedding(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::NotInitialized(p) => write!(f, "runtime root not initialized: {}", p.display()),
            Self::LockHeld(p) => write!(
                f,
                "{} already held: {} (a hard-killed run leaves this file behind; \
                 remove it only after confirming no run holds it)",
                lock_kind(p),
                p.display()
            ),
            Self::AccessDenied(m) => write!(f, "access denied: {m}"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::Integrity(m) => write!(f, "integrity check failed: {m}"),
            Self::Corrupt(m) => write!(f, "corrupt archive state: {m}"),
            Self::LimitExceeded(limit) => {
                write!(f, "object exceeds the {limit}-byte limit; nothing was archived")
            }
            Self::Embedding(message) => {
                write!(f, "embedding unavailable (lexical path continues): {message}")
            }
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<AppError> for ArchiveError {
    fn from(e: AppError) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Archive
// ---------------------------------------------------------------------------

pub struct Archive {
    root: PathBuf,
    /// Committed-manifest snapshot (parsed at most once per change) plus the
    /// object/record access indexes derived from it. See [`ManifestSnapshot`].
    snapshot: Mutex<Option<Arc<ManifestSnapshot>>>,
    /// Record ledger and source state, loaded at most once per change. A
    /// capture-all round stages both and writes them once at the end.
    ledger: Mutex<LedgerCache>,
}

/// On-disk identity of one state file (size + modification time in ms); the
/// same stamp keys the manifest listing (see `snapshot::FileStamp`).
fn stamp_of(meta: &fs::Metadata) -> FileStamp {
    FileStamp {
        len: meta.len(),
        mtime_ms: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0),
    }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    fs::metadata(path).ok().map(|meta| stamp_of(&meta))
}

/// Human name of one lock file, for [`ArchiveError::LockHeld`] messages: the
/// capture, derived, compaction and journal locks have different owners and
/// different consequences, so a generic "capture lock" text would misdirect the
/// operator who has to clear a stale file.
fn lock_kind(path: &Path) -> &'static str {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("capture.lock") => "capture lock",
        Some("derived.lock") => "derived lock",
        Some(RECORDS_COMPACT_LOCK) => "records compaction lock",
        Some(RECORDS_TAIL_LOCK) => "records journal lock",
        _ => "lock",
    }
}

/// True when a failed lock acquisition means "someone — or a file that is
/// halfway deleted — holds it", not "this I/O is broken".
///
/// Windows reports a `create_new` that lands on a file another thread is in the
/// middle of deleting as `ERROR_ACCESS_DENIED` (`PermissionDenied`) instead of
/// "already exists": the same contention, one layer lower. `acquire_lock_wait`
/// retries that class like a held lock, and when the budget runs out the caller
/// receives it unchanged — so a caller with a safe degraded path (keep the
/// journal, trim it on the next round) must classify both classes alike instead
/// of failing the round. A genuine I/O failure (missing `locks/` directory,
/// read-only volume) keeps failing the operation.
fn lock_contended(error: &ArchiveError) -> bool {
    match error {
        ArchiveError::LockHeld(_) => true,
        ArchiveError::Io(AppError::Io(_, source)) => matches!(
            source.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::AlreadyExists
        ),
        _ => false,
    }
}

/// Name of the lock that serialises compactions (merged read, canonical
/// rewrite, journal removal). A compactor holds it for the whole run, so two
/// compactors can never interleave two rewrites: the loser skips (automatic
/// path) or reports a held lock (explicit command). Appenders do **not** take
/// this lock — a compaction lasts as long as a full canonical rewrite — they
/// take [`RECORDS_TAIL_LOCK`] instead, which is only held for the journal
/// operations themselves.
pub(crate) const RECORDS_COMPACT_LOCK: &str = "records-compact.lock";

/// Name of the lock that serialises the journal's own byte-range operations:
/// the append (repair half line + `write_all` + `fsync`) on one side and a
/// compaction's stamp check + removal on the other.
///
/// Every journal **write** takes it and every journal **removal** takes it, so a
/// line can only reach the journal while no compaction is inside its removal
/// window: a compaction never deletes a byte it did not merge. The hold is one
/// repair plus one append, i.e. milliseconds; a compaction holds it for one
/// `stat`, one read of the whole journal (at most
/// `RECORDS_TAIL_COMPACT_BYTES` ≈ 1 MiB) and one `remove_file`, never across the
/// canonical rewrite.
pub(crate) const RECORDS_TAIL_LOCK: &str = "records-tail.lock";

/// Upper bound a flush waits for [`RECORDS_TAIL_LOCK`]. A live holder keeps it
/// for milliseconds, so this is only reached when the file was left behind by a
/// hard kill or a holder overran every journal operation. The flush then takes
/// the **degraded path**: it never writes the journal unlocked (a line written
/// outside the lock can be deleted inside a compaction's removal window) and
/// instead promotes the staged entries into the canonical under
/// [`RECORDS_COMPACT_LOCK`]. See `Archive::flush_records_inner`.
const RECORDS_TAIL_LOCK_WAIT_MS: u64 = 250;

/// Upper bound a compaction waits for [`RECORDS_TAIL_LOCK`] before it keeps the
/// journal (safe: the merged entries are already in the canonical, the journal
/// becomes duplicates). Short, because a long wait would hold the compaction
/// lock — and therefore every other compactor — for no gain.
const RECORDS_TAIL_TRIM_WAIT_MS: u64 = 50;

/// Poll interval while waiting for [`RECORDS_TAIL_LOCK`].
const RECORDS_TAIL_LOCK_POLL_MS: u64 = 2;

/// Attempts one merged ledger read makes when a file stamp moves under it (a
/// concurrent compaction or append). Each attempt is four `stat`s (two per
/// stamp sample) plus two reads, so the bound is cheap; a stable stamp is the
/// normal case.
const LEDGER_READ_ATTEMPTS: u32 = 8;

/// Pause between merged-ledger read attempts.
const LEDGER_READ_RETRY_MS: u64 = 1;

/// What one merged ledger read cost (counters + reports).
#[derive(Debug, Clone, Copy, Default)]
struct LedgerRead {
    /// Bytes of the canonical file.
    canonical_bytes: u64,
    /// Entries in the canonical file alone (what an old reader sees).
    canonical_records: u64,
    /// Bytes of the journal.
    tail_bytes: u64,
    /// Complete journal lines.
    tail_lines: u64,
    /// Unterminated last journal line ignored (1 when present).
    tail_incomplete_lines: u64,
    /// Journal lines skipped because the sha was already present.
    tail_duplicate_lines: u64,
    /// Entries the journal actually contributed.
    tail_inserted: u64,
    /// Digest of the exact journal bytes this read merged (`None` when the file
    /// was absent). A compaction compares it with the journal right before the
    /// removal, so it never deletes bytes it did not merge even when the size and
    /// the millisecond mtime happen to be reproducible.
    tail_hash: Option<u64>,
}

/// Read the merged record ledger: the canonical document first, then the
/// journal's lines for the keys it does not already hold (first writer wins).
///
/// A missing canonical or a missing journal is an empty ledger, not an error
/// (both files are created lazily). A corrupt canonical, or a *complete*
/// journal line that does not parse / carries a bad sha / misses a field, is an
/// error: the ledger is reported corrupt instead of silently losing entries.
///
/// # The stamp describes the bytes read, unless the file was rewritten in place
///
/// Both file stamps are sampled **before** the first byte is read and again
/// after the last one; the read only counts when the two samples are equal, so
/// no writer touched either file while it ran. Two consequences:
///
/// - a reader never mixes two states, so it cannot report a ledger that never
///   existed on disk (a compaction that rewrites the canonical and removes the
///   journal between the two reads is retried, not merged);
/// - the returned stamp is a "the merged entries are the bytes these stamps
///   describe" claim, which is what lets a compaction decide "the journal is
///   still the one I merged, so removing it loses nothing". The claim has one
///   exception: a journal that was rewritten to the **same length in the same
///   millisecond** has the same stamp. The removal therefore also compares the
///   digest of the exact bytes merged (`journal_is_the_one_merged`), and cache
///   invalidation — which has no such digest — can serve a view one load too
///   long after that shape, until any real stamp move heals it.
///
/// A stamp that moves makes the attempt restart ([`LEDGER_READ_ATTEMPTS`] times,
/// a millisecond apart). If the files never settle — a writer that modifies them
/// continuously for longer than that — the read returns the entries it merged
/// with the **older** stamp: an understated stamp only costs one more reload and
/// makes a compaction keep the journal, while an overstated one would let a
/// cached view outlive its bytes. Exhausting the attempts returns that
/// best-effort merge under the older stamp and discloses it
/// (`records_read.unstable`); the two files were read at different moments, so
/// that merge is not necessarily one state the ledger had, but every entry in it
/// is on disk.
fn read_ledger(root: &Path) -> Result<(RecordsFile, LedgerStamp, LedgerRead), ArchiveError> {
    let canonical = root.join("state").join(records::RECORDS_CANONICAL_FILENAME);
    let tail = root.join("state").join(records::RECORDS_TAIL_FILENAME);
    let mut attempt = 0u32;
    loop {
        let before = LedgerStamp::of(&canonical, &tail);
        let (ledger, read) = read_ledger_once(root, &canonical, &tail)?;
        let after = LedgerStamp::of(&canonical, &tail);
        if before == after {
            return Ok((ledger, after, read));
        }
        attempt += 1;
        crate::cost::counter("records_read.retries", 1);
        if attempt >= LEDGER_READ_ATTEMPTS {
            // Still moving: hand back the merge under the older stamp (see the
            // doc comment: the safe direction), disclosed by a counter.
            crate::cost::counter("records_read.unstable", 1);
            return Ok((ledger, before, read));
        }
        std::thread::sleep(std::time::Duration::from_millis(LEDGER_READ_RETRY_MS));
    }
}

/// One attempt of [`read_ledger`]: canonical bytes, then journal bytes.
fn read_ledger_once(
    // Only the test-only interleaving hook needs the root (`cfg(test)` below);
    // the production build reads the two files it is given.
    _root: &Path,
    canonical: &Path,
    tail: &Path,
) -> Result<(RecordsFile, LedgerRead), ArchiveError> {
    let mut read = LedgerRead::default();
    let mut ledger = records::empty_ledger();
    match fs::read(canonical) {
        Ok(raw) => {
            read.canonical_bytes = raw.len() as u64;
            ledger = serde_json::from_slice(&raw).map_err(|e| {
                ArchiveError::Corrupt(format!("{}: {e}", records::RECORDS_CANONICAL_PATH))
            })?;
            read.canonical_records = ledger.records.len() as u64;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::io(canonical, e).into()),
    }
    // Deterministic interleaving point for the concurrency tests (test builds
    // only): the place where a compaction between the two reads used to produce
    // a torn ledger.
    #[cfg(test)]
    ledger_probe::enter(ledger_probe::Point::ReadBetweenFiles, _root);
    match fs::read(tail) {
        Ok(raw) => {
            read.tail_bytes = raw.len() as u64;
            read.tail_hash = Some(records::digest(&raw));
            let parsed = records::parse_tail(&raw)?;
            read.tail_lines = parsed.lines;
            read.tail_incomplete_lines = u64::from(parsed.incomplete_bytes > 0);
            let stats = records::merge_first_writer(&mut ledger.records, parsed.entries);
            read.tail_inserted = stats.inserted;
            read.tail_duplicate_lines = stats.duplicates;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::io(tail, e).into()),
    }
    Ok((ledger, read))
}

/// In-memory view of `state/records.json` + `state/records.jsonl` and
/// `state/sources.json`.
///
/// Invalidation: a cached value is reused while the file stamps are unchanged,
/// or while this process holds unflushed changes (its own writes are newer than
/// the file).
///
/// A cached ledger is a *merged* view of both on-disk forms, and every write
/// path stores the stamp of the files it produced (see [`read_ledger`] and
/// [`Archive::flush_records_inner`]). A stamp is `(len, mtime_ms)` per file, so
/// it describes the bytes that were merged **unless** the journal was rewritten
/// to the same length in the same millisecond: that shape is blocked on the
/// destroying side by the removal's content check
/// (`journal_is_the_one_merged`), while on the visible side it can keep a cached
/// view alive for one more load; any real stamp move heals it. Another process
/// appending or compacting the ledger is a supported concurrent operation —
/// capture holds the single-writer lock against a second *capture*, not against
/// a compaction — so the stamp check is what keeps the cache honest when a CLI
/// compaction moves either file.
///
/// The record ledger has **two** on-disk forms of the same ledger (canonical
/// file + append-only journal, see [`records`]); both stamps take part in the
/// invalidation, so a round that only appended to the journal is seen by every
/// later load instead of being served from a stale cache.
#[derive(Default)]
struct LedgerCache {
    records: Option<Arc<RecordsFile>>,
    records_stamp: Option<LedgerStamp>,
    records_dirty: bool,
    /// The staged change is a whole-ledger replacement (`save_records`), not an
    /// append: the flush must rewrite the canonical and drop the journal.
    records_replace: bool,
    /// Entries staged since the last successful write, in staging order.
    records_staged: Vec<(String, RecordRef)>,
    records_appends: u64,
    records_append_bytes: u64,
    records_compactions: u64,
    records_compact_bytes: u64,
    records_promotions: u64,
    records_tail_bytes: u64,
    records_tail_incomplete_lines: u64,
    records_tail_duplicate_lines: u64,
    records_compact_errors: u64,
    records_compact_error: Option<String>,
    records_first_writer_resolved: u64,
    /// Set when a flush could not take `records-tail.lock` within its budget
    /// (a stale file left by a hard kill): the next flushes skip the wait while
    /// the lock file is still there, so a stale file costs one timeout, not one
    /// per round. The flush then promotes its entries into the canonical
    /// (`records_promotions`) instead of writing the journal unlocked.
    records_tail_lock_timeout: bool,
    sources: Option<Arc<SourcesFile>>,
    sources_stamp: Option<FileStamp>,
    sources_dirty: bool,
    /// A capture-all round is open: writes are staged and flushed once.
    deferred: bool,
    /// Load/write counters for this archive instance (test + report evidence).
    records_loads: u64,
    records_flushes: u64,
    records_read_bytes: u64,
    records_written_bytes: u64,
    sources_loads: u64,
    sources_flushes: u64,
    sources_read_bytes: u64,
    sources_written_bytes: u64,
}

/// Stamps of both on-disk forms of the record ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LedgerStamp {
    canonical: Option<FileStamp>,
    tail: Option<FileStamp>,
}

impl LedgerStamp {
    fn of(canonical: &Path, tail: &Path) -> LedgerStamp {
        LedgerStamp {
            canonical: file_stamp(canonical),
            tail: file_stamp(tail),
        }
    }
}

/// Outcome of one `compact-records` run (also the explicit CLI report).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactRecordsReport {
    /// Record count an old reader (canonical file only) saw before the run.
    pub records_before: u64,
    /// Record count of the merged ledger (what every reader sees now).
    pub records_after: u64,
    /// Journal file size before the run.
    pub tail_bytes: u64,
    /// Complete journal lines read.
    pub tail_lines: u64,
    /// Unterminated last line ignored by this run (append crash window).
    pub tail_incomplete_lines: u64,
    /// Journal lines skipped because the sha was already present.
    pub tail_duplicate_lines: u64,
    /// Bytes written to the canonical file (0 for a dry run).
    pub bytes_written: u64,
    /// Whether a compaction actually ran.
    pub performed: bool,
    /// Why nothing was written (dry run, nothing to compact, capture session).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// Whether the journal was removed after the canonical write.
    pub tail_removed: bool,
    /// True when the journal changed between the read and the removal attempt;
    /// it is kept, so nothing appended in between can be lost.
    pub tail_kept_because_changed: bool,
    /// True when the journal lock (`locks/records-tail.lock`) could not be
    /// taken, so the journal was kept: the canonical holds every merged entry
    /// and the journal is now only duplicates. Cleared by removing a stale lock
    /// file (see the README's concurrency section).
    pub tail_kept_because_locked: bool,
    /// Canonical bytes after the run (0 when it does not exist).
    pub canonical_bytes: u64,
}

impl Archive {
    /// Open an initialized runtime root (fail-closed when missing).
    pub fn open(root: &Path) -> Result<Self, ArchiveError> {
        if !root.join("state").join("schema.json").exists() {
            return Err(ArchiveError::NotInitialized(root.to_path_buf()));
        }
        Ok(Self {
            root: root.to_path_buf(),
            snapshot: Mutex::new(None),
            ledger: Mutex::new(LedgerCache::default()),
        })
    }

    /// Create the runtime root layout. Existing roots are reused untouched.
    pub fn init(root: &Path) -> Result<Self, ArchiveError> {
        for dir in [
            "archive/objects",
            "archive/manifests",
            "archive/staging",
            "state",
            "derived",
            "locks",
            "logs",
        ] {
            fs::create_dir_all(root.join(dir)).map_err(|e| AppError::io(&root.join(dir), e))?;
        }
        let schema_path = root.join("state").join("schema.json");
        if !schema_path.exists() {
            let schema = SchemaFile {
                schema_version: ARCHIVE_SCHEMA_VERSION,
                derived: DerivedVersion {
                    parser_version: env!("CARGO_PKG_VERSION").to_string(),
                    model_version: DERIVED_MODEL_VERSION,
                },
            };
            atomic_write(
                &schema_path,
                serde_json::to_string_pretty(&schema)
                    .map_err(|e| AppError::message(e.to_string()))?
                    .as_bytes(),
            )?;
        }
        let device_path = root.join("state").join("device_id");
        if !device_path.exists() {
            let id = new_id("device");
            atomic_write(&device_path, id.as_bytes())?;
        }
        Self::open(root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn device_id(&self) -> Result<String, ArchiveError> {
        let path = self.root.join("state").join("device_id");
        let raw = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
        Ok(raw.trim().to_string())
    }

    pub fn schema(&self) -> Result<SchemaFile, ArchiveError> {
        let path = self.root.join("state").join("schema.json");
        let raw = fs::read(&path).map_err(|e| AppError::io(&path, e))?;
        serde_json::from_slice(&raw)
            .map_err(|e| ArchiveError::Corrupt(format!("state/schema.json: {e}")))
    }

    // -- single-writer lock -------------------------------------------------

    /// Acquire the runtime-root single-writer lock. Explicitly refuses when
    /// another capture instance holds it.
    ///
    /// The lock is a `create_new` sentinel file released on unwinding. A
    /// *hard* kill (process terminated, no unwinding) leaves the file behind and
    /// every later run refuses with [`ArchiveError::LockHeld`]; the file records
    /// `pid` + acquisition time so an operator can confirm no capture is running
    /// before removing it. Stale-lock reclamation is deliberately not automatic:
    /// guessing liveness without a process-start identity risks two writers.
    pub fn acquire_lock(&self) -> Result<LockGuard, ArchiveError> {
        self.acquire_named_lock("capture.lock")
    }

    /// Serialize derived replays (a separate writer domain from capture). The
    /// derived layer is not evidence, so it must never make a capture wait; it
    /// must also never be rebuilt by two processes at once.
    pub fn acquire_derived_lock(&self) -> Result<LockGuard, ArchiveError> {
        self.acquire_named_lock("derived.lock")
    }

    fn acquire_named_lock(&self, name: &str) -> Result<LockGuard, ArchiveError> {
        let path = self.root.join("locks").join(name);
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "pid={} acquired_at_unix_ms={}", std::process::id(), now_ms());
                let _ = f.sync_all();
                Ok(LockGuard { path })
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Err(ArchiveError::LockHeld(path))
            }
            Err(e) => Err(ArchiveError::Io(AppError::io(&path, e))),
        }
    }

    /// Acquire a named lock, polling for at most `wait_ms`; a lock that is still
    /// held after the budget reports [`ArchiveError::LockHeld`].
    ///
    /// Used where a *live* holder only keeps the lock for a short, bounded
    /// operation (the journal lock) and a stale file must not stop the caller:
    /// the tail lock is held for one append or one `remove_file`, never for a
    /// canonical rewrite.
    ///
    /// Windows reports a `create_new` on a file another thread is in the middle
    /// of deleting as `ERROR_ACCESS_DENIED` instead of "already exists", so an
    /// I/O error is retried like a held lock and only surfaced when the budget
    /// runs out (or immediately for a genuinely permanent one, which then costs
    /// one budget of polling).
    fn acquire_lock_wait(&self, name: &str, wait_ms: u64) -> Result<LockGuard, ArchiveError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        loop {
            match self.acquire_named_lock(name) {
                Ok(guard) => return Ok(guard),
                Err(error) if std::time::Instant::now() < deadline => {
                    if !matches!(error, ArchiveError::LockHeld(_) | ArchiveError::Io(_)) {
                        return Err(error);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(
                        RECORDS_TAIL_LOCK_POLL_MS,
                    ));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Path of the journal lock file (`locks/records-tail.lock`).
    fn records_tail_lock_file(&self) -> PathBuf {
        self.root.join("locks").join(RECORDS_TAIL_LOCK)
    }

    /// Whether the journal on disk is still byte-for-byte the one `read` merged.
    ///
    /// Called under [`RECORDS_TAIL_LOCK`], right before the removal, so no append
    /// can change the file in between. A missing journal counts as "the one
    /// merged" only when the read saw no journal either.
    fn journal_is_the_one_merged(
        &self,
        tail: &Path,
        read: &LedgerRead,
    ) -> Result<bool, ArchiveError> {
        match fs::read(tail) {
            Ok(raw) => Ok(raw.len() as u64 == read.tail_bytes
                && read.tail_hash == Some(records::digest(&raw))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(read.tail_hash.is_none()),
            Err(error) => Err(AppError::io(tail, error).into()),
        }
    }

    /// Degraded flush: put the staged entries into the canonical instead of the
    /// journal, when [`RECORDS_TAIL_LOCK`] could not be taken.
    ///
    /// Writing the journal without that lock is the one thing this crate must
    /// never do: a line can land between a compaction's "the journal is still
    /// the one I merged" check and its `remove_file`, and is then deleted before
    /// anything merged it. The canonical write below needs no journal lock,
    /// because it never removes the journal and takes [`RECORDS_COMPACT_LOCK`],
    /// which a compaction holds for its entire run — canonical rewrite and
    /// removal included. So no removal can be concurrent with this write, and a
    /// line another writer appends after this read simply stays in the journal
    /// (still merged by every reader, still removed only by a later compaction
    /// that merges it).
    ///
    /// The write is a full canonical rewrite: `save_records`-sized, not
    /// append-sized. That is the documented cost of a journal lock left behind
    /// by a hard kill; the alternative is either a lost entry or a capture round
    /// that can never make progress.
    fn promote_staged_records(
        &self,
        cache: &mut LedgerCache,
        staged: &[(String, RecordRef)],
    ) -> Result<(), ArchiveError> {
        // A live compaction holds this for its whole run (a full canonical
        // rewrite), so the wait is the same short budget as the journal lock:
        // past it the round reports the held lock, keeps its staged entries and
        // leaves the source watermark where it was (see `CaptureSession`), so
        // the next round re-captures and re-stages instead of losing them.
        let _lock = self.acquire_lock_wait(RECORDS_COMPACT_LOCK, RECORDS_TAIL_LOCK_WAIT_MS)?;
        let canonical = self.records_canonical_path();
        let tail = self.records_tail_path();
        let (mut ledger, stamp, _read) = read_ledger(&self.root)?;
        for (sha, reference) in staged {
            ledger
                .records
                .entry(sha.clone())
                .or_insert_with(|| reference.clone());
        }
        if ledger.schema.is_empty() {
            ledger.schema = RECORDS_SCHEMA.to_string();
        }
        let data = serde_json::to_string_pretty(&ledger)
            .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        let _phase = crate::cost::phase("records_promote");
        atomic_write(&canonical, data.as_bytes())?;
        // The stamp must not claim more than this cache holds: if another writer
        // appended to the journal after the read above, that line is on disk but
        // not in `ledger`, so the pre-write stamp is stored instead (an
        // understated stamp only costs one reload). Both choices are stamp pairs
        // the files really had.
        let stamp_after = LedgerStamp::of(&canonical, &tail);
        let stamp_after = if stamp_after.tail == stamp.tail {
            stamp_after
        } else {
            stamp
        };
        cache.records = Some(Arc::new(ledger));
        cache.records_stamp = Some(stamp_after);
        cache.records_dirty = false;
        cache.records_staged.clear();
        cache.records_promotions = cache.records_promotions.saturating_add(1);
        crate::cost::counter("records_write.promoted", 1);
        crate::cost::counter("records_write.promote_bytes", data.len() as u64);
        crate::cost::counter("records_write.promote_entries", staged.len() as u64);
        Ok(())
    }

    pub fn lock_held(&self) -> bool {
        self.root.join("locks").join("capture.lock").exists()
    }

    // -- object store --------------------------------------------------------

    /// Plain-encoding path of an object (without suffix check — use
    /// [`Archive::object_encoding`] to ask what is on disk).
    fn object_path(&self, sha256: &str) -> PathBuf {
        self.root
            .join("archive")
            .join("objects")
            .join(&sha256[0..2])
            .join(&sha256[2..4])
            .join(sha256)
    }

    fn object_zstd_path(&self, sha256: &str) -> PathBuf {
        let mut path = self.object_path(sha256).into_os_string();
        path.push(ZSTD_OBJECT_SUFFIX);
        PathBuf::from(path)
    }

    pub(crate) fn object_path_for(&self, sha256: &str, encoding: ObjectEncoding) -> PathBuf {
        match encoding {
            ObjectEncoding::Plain => self.object_path(sha256),
            ObjectEncoding::Zstd => self.object_zstd_path(sha256),
        }
    }

    /// Explicit storage encoding of an object on disk, by file name — never by
    /// content sniffing. When both encodings exist (a `compact` crash window)
    /// the zstd file wins: it was read-back verified before its atomic rename,
    /// and the plain leftover is removed by the next `compact`.
    pub fn object_encoding(&self, sha256: &str) -> Option<ObjectEncoding> {
        if !is_valid_sha256(sha256) {
            return None;
        }
        if self.object_zstd_path(sha256).exists() {
            return Some(ObjectEncoding::Zstd);
        }
        if self.object_path(sha256).exists() {
            return Some(ObjectEncoding::Plain);
        }
        None
    }

    /// True when the object exists in *either* storage encoding.
    pub fn has_object(&self, sha256: &str) -> bool {
        self.object_encoding(sha256).is_some()
    }

    /// Every object file on disk, sorted by (tier1, tier2, name). Invalid file
    /// names (not a 64-hex sha, with or without the zstd suffix) are skipped
    /// here; `reconcile` reports them as orphans.
    pub fn object_files(&self) -> Result<Vec<ObjectFileEntry>, ArchiveError> {
        let objects_dir = self.root.join("archive").join("objects");
        let mut out = Vec::new();
        for tier1 in read_dir_sorted(&objects_dir)? {
            if !tier1.path().is_dir() {
                continue;
            }
            for tier2 in read_dir_sorted(&tier1.path())? {
                if !tier2.path().is_dir() {
                    continue;
                }
                for obj in read_dir_sorted(&tier2.path())? {
                    let name = obj.file_name().to_string_lossy().to_string();
                    let (sha, encoding) = match name.strip_suffix(ZSTD_OBJECT_SUFFIX) {
                        Some(sha) => (sha, ObjectEncoding::Zstd),
                        None => (name.as_str(), ObjectEncoding::Plain),
                    };
                    if !is_valid_sha256(sha) {
                        continue;
                    }
                    let stored_bytes = obj
                        .metadata()
                        .map_err(|e| AppError::io(&obj.path(), e))?
                        .len();
                    out.push(ObjectFileEntry {
                        sha256: sha.to_string(),
                        encoding,
                        path: obj.path(),
                        stored_bytes,
                    });
                }
            }
        }
        Ok(out)
    }

    /// S2.5: per-encoding storage statistics over the object store.
    ///
    /// Cheap by design (capture calls `reconcile`, which includes these stats,
    /// after every run): `stored_bytes` comes from file metadata; zstd
    /// *original* bytes come from the committed manifests (every referenced
    /// object's decoded size is a capture fact). Only zstd objects that no
    /// manifest references (crash-window orphans) are decoded, bounded by
    /// [`OBJECT_CHUNK_BYTES`] of memory each.
    pub fn storage_stats(&self) -> Result<StorageStats, ArchiveError> {
        let mut referenced: BTreeMap<String, u64> = BTreeMap::new();
        for manifest in self.manifests()?.iter() {
            for obj in &manifest.objects {
                referenced.entry(obj.sha256.clone()).or_insert(obj.bytes);
            }
        }
        let mut stats = StorageStats::default();
        for entry in self.object_files()? {
            let (objects, original, stored) = match entry.encoding {
                ObjectEncoding::Plain => (
                    &mut stats.plain_objects,
                    &mut stats.plain_original_bytes,
                    &mut stats.plain_stored_bytes,
                ),
                ObjectEncoding::Zstd => (
                    &mut stats.zstd_objects,
                    &mut stats.zstd_original_bytes,
                    &mut stats.zstd_stored_bytes,
                ),
            };
            *objects = objects.saturating_add(1);
            *stored = stored.saturating_add(entry.stored_bytes);
            let original_len = match referenced.get(&entry.sha256) {
                Some(bytes) => *bytes,
                None => match entry.encoding {
                    ObjectEncoding::Plain => entry.stored_bytes,
                    ObjectEncoding::Zstd => match decoded_len_of(&entry.path) {
                        Ok(len) => len,
                        Err(_) => {
                            stats.unreadable_objects = stats.unreadable_objects.saturating_add(1);
                            0
                        }
                    },
                },
            };
            *original = original.saturating_add(original_len);
        }
        Ok(stats)
    }

    /// Store bytes as a content-addressed object (plain encoding). Returns
    /// (sha256, bytes, is_new). Write goes through `archive/staging/` and is
    /// renamed into place only after the hash is known; existing objects are
    /// never rewritten. This is the entry point for objects whose bytes are
    /// already in their final domain encoding (e.g. zstd payloads produced by
    /// the SQLite collector) — it never compresses.
    pub fn put_object(&self, bytes: &[u8]) -> Result<(String, u64, bool), ArchiveError> {
        let (sha, bytes, is_new, _) = self.put_object_encoded(bytes, StorageHint::Raw)?;
        Ok((sha, bytes, is_new))
    }

    /// S2.5: store bytes with an explicit storage hint. The content address is
    /// always the sha256 of the *decoded* bytes; the hint only picks the
    /// on-disk encoding. For [`StorageHint::Compressible`] the write order is
    /// compress → staging file → read back + decode + verify sha256 → atomic
    /// rename; the manifest commit order downstream is unchanged.
    /// Returns (sha256, decoded bytes, is_new, encoding).
    pub fn put_object_encoded(
        &self,
        bytes: &[u8],
        hint: StorageHint,
    ) -> Result<(String, u64, bool, ObjectEncoding), ArchiveError> {
        let sha = hex_sha256(bytes);
        if let Some(encoding) = self.object_encoding(&sha) {
            return Ok((sha, bytes.len() as u64, false, encoding));
        }
        let encoding = match hint {
            StorageHint::Raw => ObjectEncoding::Plain,
            StorageHint::Compressible => {
                let compressed = zstd::stream::encode_all(bytes, ZSTD_COMPRESSION_LEVEL)
                    .map_err(|e| ArchiveError::Corrupt(format!("object compression: {e}")))?;
                if worth_compressing(bytes.len() as u64, compressed.len() as u64) {
                    // Compress → staging file → decode-readback verify → rename.
                    let staging = self.staging_path();
                    let staged = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&staging)
                        .map_err(|e| AppError::io(&staging, e))
                        .and_then(|mut file| {
                            file.write_all(&compressed)
                                .and_then(|_| file.sync_all())
                                .map_err(|e| AppError::io(&staging, e))
                        });
                    if let Err(e) = staged {
                        let _ = fs::remove_file(&staging);
                        return Err(e.into());
                    }
                    if let Err(e) = verify_staged_zstd(&staging, &sha, bytes.len() as u64) {
                        let _ = fs::remove_file(&staging);
                        return Err(e);
                    }
                    return self.finalize_staged_object(staging, sha, bytes.len() as u64, ObjectEncoding::Zstd);
                }
                ObjectEncoding::Plain
            }
        };
        if encoding == ObjectEncoding::Plain {
            let staging = self.staging_path();
            {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&staging)
                    .map_err(|e| AppError::io(&staging, e))?;
                let written = file
                    .write_all(bytes)
                    .and_then(|_| file.sync_all());
                if let Err(e) = written {
                    drop(file);
                    let _ = fs::remove_file(&staging);
                    return Err(AppError::io(&staging, e).into());
                }
            }
            return self.finalize_staged_object(staging, sha, bytes.len() as u64, ObjectEncoding::Plain);
        }
        unreachable!("compressible branch returns early")
    }

    /// P4: store an arbitrarily large stream as a content-addressed object without
    /// materializing it in memory (plain encoding — see
    /// [`Archive::put_object_stream_encoded`] for the compressing variant).
    ///
    /// The stream is copied into `archive/staging/` in [`OBJECT_CHUNK_BYTES`]
    /// chunks while SHA-256 is updated incrementally. Exceeding `max_bytes`
    /// aborts the write, removes the staging file and returns
    /// [`ArchiveError::LimitExceeded`] — an over-limit object is never partially
    /// committed and never becomes an archive object.
    pub fn put_object_stream<R: Read>(
        &self,
        reader: &mut R,
        max_bytes: u64,
    ) -> Result<(String, u64, bool), ArchiveError> {
        let staging = self.staging_path();
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buffer = vec![0u8; OBJECT_CHUNK_BYTES];
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)
                .map_err(|e| AppError::io(&staging, e))?;
            loop {
                let read = match reader.read(&mut buffer) {
                    Ok(read) => read,
                    Err(e) => {
                        drop(file);
                        let _ = fs::remove_file(&staging);
                        return Err(AppError::io(&staging, e).into());
                    }
                };
                if read == 0 {
                    break;
                }
                total = total.saturating_add(read as u64);
                if total > max_bytes {
                    drop(file);
                    let _ = fs::remove_file(&staging);
                    return Err(ArchiveError::LimitExceeded(max_bytes));
                }
                hasher.update(&buffer[..read]);
                if let Err(e) = file.write_all(&buffer[..read]) {
                    drop(file);
                    let _ = fs::remove_file(&staging);
                    return Err(AppError::io(&staging, e).into());
                }
            }
            file.sync_all().map_err(|e| AppError::io(&staging, e))?;
        }
        let sha = hex_lower(&hasher.finalize());
        self.finalize_staged_object(staging, sha, total, ObjectEncoding::Plain)
            .map(|(sha, bytes, is_new, _)| (sha, bytes, is_new))
    }

    /// S2.5: stream-store an object with an explicit storage hint. Memory use
    /// stays bounded by [`OBJECT_CHUNK_BYTES`] regardless of object size.
    ///
    /// With [`StorageHint::Compressible`] the stream is compressed into a
    /// staging file while the raw hash is computed; after the stream ends the
    /// encoded size decides the encoding. When compression is not worth it the
    /// reader is rewound ([`RestartableRead`]) and a second pass stores the
    /// plain bytes, so no object is ever stored in a form bigger than its
    /// original. The compressed staging file is decoded and hash-verified from
    /// disk before it is renamed into place.
    /// Returns (sha256, decoded bytes, is_new, encoding).
    pub fn put_object_stream_encoded<R: RestartableRead>(
        &self,
        reader: &mut R,
        max_bytes: u64,
        hint: StorageHint,
    ) -> Result<(String, u64, bool, ObjectEncoding), ArchiveError> {
        let staging = self.staging_path();
        let mut raw_hasher = Sha256::new();
        let mut raw_total = 0u64;
        let encoding = match hint {
            StorageHint::Raw => ObjectEncoding::Plain,
            StorageHint::Compressible => {
                // First pass: compress into the staging file while hashing raw bytes.
                let mut limit: Option<u64> = None;
                {
                    let file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&staging)
                        .map_err(|e| AppError::io(&staging, e))?;
                    let mut encoder = zstd::stream::write::Encoder::new(file, ZSTD_COMPRESSION_LEVEL)
                        .map_err(|e| ArchiveError::Corrupt(format!("object compression: {e}")))?;
                    let mut buffer = vec![0u8; OBJECT_CHUNK_BYTES];
                    loop {
                        let read = reader.read(&mut buffer).map_err(|e| {
                            let _ = fs::remove_file(&staging);
                            AppError::io(&staging, e)
                        });
                        let read = match read {
                            Ok(read) => read,
                            Err(e) => return Err(e.into()),
                        };
                        if read == 0 {
                            break;
                        }
                        raw_total = raw_total.saturating_add(read as u64);
                        if raw_total > max_bytes {
                            limit = Some(max_bytes);
                            break;
                        }
                        raw_hasher.update(&buffer[..read]);
                        if let Err(e) = encoder.write_all(&buffer[..read]) {
                            let _ = fs::remove_file(&staging);
                            return Err(AppError::io(&staging, e).into());
                        }
                    }
                    match encoder.finish() {
                        Ok(file) => {
                            if limit.is_none() {
                                file.sync_all().map_err(|e| AppError::io(&staging, e))?;
                            }
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&staging);
                            return Err(ArchiveError::Corrupt(format!("object compression: {e}")));
                        }
                    }
                }
                if let Some(max_bytes) = limit {
                    let _ = fs::remove_file(&staging);
                    return Err(ArchiveError::LimitExceeded(max_bytes));
                }
                let compressed_total = fs::metadata(&staging)
                    .map(|m| m.len())
                    .map_err(|e| AppError::io(&staging, e))?;
                if worth_compressing(raw_total, compressed_total) {
                    ObjectEncoding::Zstd
                } else {
                    // Not worth it: drop the compressed staging file and fall
                    // through to the plain second pass below.
                    let _ = fs::remove_file(&staging);
                    ObjectEncoding::Plain
                }
            }
        };
        if encoding == ObjectEncoding::Zstd {
            let sha = hex_lower(&raw_hasher.finalize());
            if let Some(encoding) = self.object_encoding(&sha) {
                // Lost the race with an existing object; dedupe.
                let _ = fs::remove_file(&staging);
                return Ok((sha, raw_total, false, encoding));
            }
            verify_staged_zstd(&staging, &sha, raw_total)?;
            return self.finalize_staged_object(staging, sha, raw_total, ObjectEncoding::Zstd);
        }
        // Plain path: the compressing first pass consumed the reader, so rewind
        // it and reuse the single-pass plain writer.
        reader
            .reset()
            .map_err(|e| AppError::io(&staging, e))?;
        self.put_object_stream(reader, max_bytes)
            .map(|(sha, bytes, is_new)| (sha, bytes, is_new, ObjectEncoding::Plain))
    }

    /// Move a fully written, fsynced staging file into its content-addressed
    /// location, deduplicating against an existing object in *either* encoding
    /// (one sha never ends up with two encodings through the capture path).
    fn finalize_staged_object(
        &self,
        staging: PathBuf,
        sha: String,
        bytes: u64,
        encoding: ObjectEncoding,
    ) -> Result<(String, u64, bool, ObjectEncoding), ArchiveError> {
        if let Some(existing) = self.object_encoding(&sha) {
            // Lost a race with another writer holding a different root view; dedupe.
            let _ = fs::remove_file(&staging);
            return Ok((sha, bytes, false, existing));
        }
        let final_path = self.object_path_for(&sha, encoding);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }
        // Windows AV/indexer may briefly hold the staging file open; retry.
        let mut last_err = None;
        for _ in 0..20 {
            match fs::rename(&staging, &final_path) {
                Ok(()) => return Ok((sha, bytes, true, encoding)),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => return Err(AppError::io(&final_path, e).into()),
            }
        }
        Err(AppError::io(&final_path, last_err.expect("retry loop ran")).into())
    }

    /// S2.5 (`compact`): move a verified staging file into its encoded object
    /// location. Returns `true` when this call placed the file, `false` when
    /// the target already existed (a concurrent or previous compact won; the
    /// staging copy is discarded). Unlike [`Archive::finalize_staged_object`]
    /// this does not dedupe against the *other* encoding: `compact` owns the
    /// migration of one sha and the caller removes the plain file afterwards.
    pub(crate) fn place_object_file(
        &self,
        staging: PathBuf,
        sha256: &str,
        encoding: ObjectEncoding,
    ) -> Result<bool, ArchiveError> {
        let final_path = self.object_path_for(sha256, encoding);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }
        if final_path.exists() {
            let _ = fs::remove_file(&staging);
            return Ok(false);
        }
        let mut last_err = None;
        for _ in 0..20 {
            match fs::rename(&staging, &final_path) {
                Ok(()) => return Ok(true),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => return Err(AppError::io(&final_path, e).into()),
            }
        }
        Err(AppError::io(&final_path, last_err.expect("retry loop ran")).into())
    }

    /// P4: re-read an object in bounded chunks and verify its content address
    /// over the *decoded* bytes. Returns the decoded byte length. Memory use is
    /// independent of object size.
    pub fn verify_object_stream(&self, sha256: &str) -> Result<u64, ArchiveError> {
        if !is_valid_sha256(sha256) {
            return Err(ArchiveError::NotFound(format!("object {sha256}")));
        }
        let encoding = self
            .object_encoding(sha256)
            .ok_or_else(|| ArchiveError::NotFound(format!("object {sha256}")))?;
        let path = self.object_path_for(sha256, encoding);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(ArchiveError::NotFound(format!("object {sha256}")))
            }
            Err(e) => return Err(AppError::io(&path, e).into()),
        };
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; OBJECT_CHUNK_BYTES];
        let mut total = 0u64;
        let mut reader: Box<dyn Read> = match encoding {
            ObjectEncoding::Plain => Box::new(file),
            ObjectEncoding::Zstd => Box::new(
                zstd::stream::read::Decoder::new(file)
                    .map_err(|e| ArchiveError::Integrity(format!("object {sha256} is not valid zstd: {e}")))?,
            ),
        };
        loop {
            let read = reader.read(&mut buffer).map_err(|e| AppError::io(&path, e))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            hasher.update(&buffer[..read]);
        }
        let actual = hex_lower(&hasher.finalize());
        if actual != sha256 {
            return Err(ArchiveError::Integrity(format!(
                "object {sha256} hashes to {actual}"
            )));
        }
        Ok(total)
    }

    pub(crate) fn staging_path(&self) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.root
            .join("archive")
            .join("staging")
            .join(format!("obj.{}.{}.{}", std::process::id(), ts, n))
    }

    /// Read an object's decoded bytes and verify against its content address.
    /// The storage encoding is taken from the file name (see
    /// [`Archive::object_encoding`]); this is the single decode point every
    /// reader goes through.
    pub fn read_object_bytes(&self, sha256: &str) -> Result<Vec<u8>, ArchiveError> {
        if !is_valid_sha256(sha256) {
            // A malformed address is "not found", never a panic or a path escape.
            return Err(ArchiveError::NotFound(format!("object {sha256}")));
        }
        let encoding = self
            .object_encoding(sha256)
            .ok_or_else(|| ArchiveError::NotFound(format!("object {sha256}")))?;
        let path = self.object_path_for(sha256, encoding);
        let mut data = Vec::new();
        fs::File::open(&path)
            .and_then(|mut f| f.read_to_end(&mut data))
            .map_err(|e| AppError::io(&path, e))?;
        let decoded = match encoding {
            ObjectEncoding::Plain => data,
            ObjectEncoding::Zstd => zstd::stream::decode_all(data.as_slice()).map_err(|e| {
                ArchiveError::Integrity(format!("object {sha256} is not valid zstd: {e}"))
            })?,
        };
        let actual = hex_sha256(&decoded);
        if actual != sha256 {
            return Err(ArchiveError::Integrity(format!(
                "object {sha256} hashes to {actual}"
            )));
        }
        Ok(decoded)
    }

    // -- manifests -----------------------------------------------------------

    /// Commit a manifest atomically (unique filename; write + sync + rename).
    pub fn commit_manifest(&self, manifest: &Manifest) -> Result<PathBuf, ArchiveError> {
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(ArchiveError::Corrupt(format!(
                "unsupported manifest schema {}",
                manifest.schema
            )));
        }
        let seq = self.next_manifest_seq()?;
        let name = format!("{:08}-{}.json", seq, manifest.capture_id);
        let path = self.root.join("archive").join("manifests").join(&name);
        let data = serde_json::to_string_pretty(manifest)
            .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        atomic_write(&path, data.as_bytes())?;
        Ok(path)
    }

    fn next_manifest_seq(&self) -> Result<u64, ArchiveError> {
        let mut max_seq = 0u64;
        for (name, _) in self.list_manifest_files()? {
            if let Some(seq) = name.split('-').next().and_then(|s| s.parse::<u64>().ok()) {
                max_seq = max_seq.max(seq);
            }
        }
        Ok(max_seq + 1)
    }

    /// The manifest directory listing with each file's `(size, mtime)`: the
    /// snapshot key.
    ///
    /// `DirEntry::metadata` is served from the directory enumeration on Windows
    /// (no per-file open), so a refresh stays one listing plus bookkeeping; on
    /// other platforms it is one `lstat` per manifest.
    fn manifest_files(&self) -> Result<Vec<ManifestFile>, ArchiveError> {
        let dir = self.root.join("archive").join("manifests");
        let mut out = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(AppError::io(&dir, e).into()),
        };
        for entry in entries {
            let entry = entry.map_err(|e| AppError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.contains(".tmp.") {
                continue;
            }
            let path = entry.path();
            let stamp = match entry.metadata() {
                Ok(meta) => stamp_of(&meta),
                // A file that vanished between the listing and the stat is not
                // part of this listing any more; the next refresh sees the new
                // listing and rebuilds.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(AppError::io(&path, e).into()),
            };
            out.push(ManifestFile { name, path, stamp });
        }
        out.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(out)
    }

    fn list_manifest_files(&self) -> Result<Vec<(String, PathBuf)>, ArchiveError> {
        Ok(self
            .manifest_files()?
            .into_iter()
            .map(|file| (file.name, file.path))
            .collect())
    }

    /// Sorted manifest file names (a directory listing - bodies are never
    /// read). Names embed the unique capture id and are append-only, so the
    /// sorted name set identifies the committed evidence set; used for
    /// change detection without parsing every manifest.
    pub fn manifest_names(&self) -> Result<Vec<String>, ArchiveError> {
        Ok(self
            .list_manifest_files()?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    /// All committed manifests, oldest first.
    ///
    /// Shared snapshot: the manifest set is parsed at most once per change of
    /// the manifest directory listing (names **and** per-file size / mtime; see
    /// [`ManifestSnapshot`]). The returned handle dereferences to the manifest
    /// slice, so `manifests()?.iter()`, `manifests()?.len()` and
    /// `&manifests[..]` keep working.
    pub fn manifests(&self) -> Result<Arc<ManifestSnapshot>, ArchiveError> {
        let files = self.manifest_files()?;
        note_list();
        crate::cost::counter("manifest_snapshot.lists", 1);
        let mut guard = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(snapshot) = guard.as_ref() {
            if snapshot.matches(&files) {
                return Ok(Arc::clone(snapshot));
            }
        }
        let snapshot = match guard.take() {
            Some(previous) => match previous.appended_after(&files).map(<[ManifestFile]>::to_vec) {
                // Append-only change: parse just the new manifests. The in-place
                // extension needs the only handle; when a caller still holds the
                // previous snapshot the same work is done on a copy (no
                // re-parse, one struct copy) and counted.
                Some(new_files) => match Arc::try_unwrap(previous) {
                    Ok(mut snapshot) => {
                        snapshot.extend(&new_files)?;
                        snapshot
                    }
                    Err(shared) => {
                        note_copy();
                        crate::cost::counter("manifest_snapshot.copies", 1);
                        let mut copy = shared.as_ref().clone();
                        copy.extend(&new_files)?;
                        copy
                    }
                },
                None => ManifestSnapshot::parse(&files)?,
            },
            None => ManifestSnapshot::parse(&files)?,
        };
        let snapshot = Arc::new(snapshot);
        *guard = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// Capture history for one physical source path, oldest first, projected
    /// from the committed manifests.
    ///
    /// P4 attachment manifests declare `source.kind = "attachment"`: they are
    /// attachment evidence units, not watermarked captures of the physical
    /// source, so they are excluded here (and reported by the attachment
    /// coverage block instead). Nothing is hidden — they have their own surface.
    pub fn source_captures(&self, source: &str) -> Result<Vec<CaptureSummary>, ArchiveError> {
        let mut out = Vec::new();
        for manifest in self.manifests()?.iter() {
            if manifest.source.path != source || manifest.source.kind == ATTACHMENT_SOURCE_KIND {
                continue;
            }
            out.push(CaptureSummary {
                capture_id: manifest.capture_id.clone(),
                captured_at_ms: manifest.captured_at_ms,
                capture_reason: manifest.source.capture_reason.clone(),
                generation: manifest.source.generation,
                range_start: manifest.source.range_start,
                records: manifest.records.len() as u64,
                bytes: manifest.objects.iter().map(|o| o.bytes).sum(),
            });
        }
        Ok(out)
    }

    // -- attachments (P4) ------------------------------------------------------

    /// Every ContentRef -> object mapping, oldest manifest first.
    pub fn attachment_entries(&self) -> Result<Vec<AttachmentSummary>, ArchiveError> {
        let mut out = Vec::new();
        for manifest in self.manifests()?.iter() {
            for entry in &manifest.attachments {
                let readable = entry.status == ATTACHMENT_STATUS_ARCHIVED
                    && entry
                        .object_sha256
                        .as_deref()
                        .map(|sha| self.has_object(sha))
                        .unwrap_or(false);
                out.push(AttachmentSummary {
                    attachment_id: attachment_id(&manifest.capture_id, entry.seq),
                    capture_id: manifest.capture_id.clone(),
                    captured_at_ms: manifest.captured_at_ms,
                    provider: manifest.provider.clone(),
                    source: manifest.source.path.clone(),
                    content_ref: entry.content_ref.clone(),
                    target: entry.target.clone(),
                    read_mode: entry.read_mode.clone(),
                    resolved_path: entry.resolved_path.clone(),
                    media_type: entry.media_type.clone(),
                    status: entry.status.clone(),
                    sha256: entry.object_sha256.clone(),
                    bytes: entry.bytes,
                    observed_bytes: entry.observed_bytes,
                    access: entry.access.clone(),
                    deduplicated: entry.deduplicated,
                    limit_bytes: entry.limit_bytes,
                    error: entry.error.clone(),
                    readable,
                });
            }
        }
        Ok(out)
    }

    /// Look up one committed attachment entry by capture id + sequence.
    pub fn find_attachment(
        &self,
        capture_id: &str,
        seq: u64,
    ) -> Result<Option<(String, ManifestAttachment)>, ArchiveError> {
        for manifest in self.manifests()?.iter() {
            if manifest.capture_id != capture_id {
                continue;
            }
            if let Some(entry) = manifest.attachments.iter().find(|a| a.seq == seq) {
                return Ok(Some((manifest.source.path.clone(), entry.clone())));
            }
        }
        Ok(None)
    }

    // -- state (watermarks, record index) -------------------------------------

    pub fn load_sources(&self) -> Result<SourcesFile, ArchiveError> {
        Ok((*self.sources_snapshot()?).clone())
    }

    fn sources_snapshot(&self) -> Result<Arc<SourcesFile>, ArchiveError> {
        let path = self.root.join("state").join("sources.json");
        let mut cache = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sources) = cache.sources.as_ref() {
            if cache.sources_dirty || file_stamp(&path) == cache.sources_stamp {
                return Ok(Arc::clone(sources));
            }
        }
        let _phase = crate::cost::phase("sources_read");
        let (sources, stamp) = match fs::read(&path) {
            Ok(raw) => (
                serde_json::from_slice(&raw)
                    .map_err(|e| ArchiveError::Corrupt(format!("state/sources.json: {e}")))?,
                file_stamp(&path),
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (
                SourcesFile {
                    schema: STATE_SCHEMA.to_string(),
                    sources: BTreeMap::new(),
                },
                None,
            ),
            Err(e) => return Err(AppError::io(&path, e).into()),
        };
        cache.sources = Some(Arc::new(sources));
        cache.sources_stamp = stamp;
        cache.sources_loads += 1;
        cache.sources_read_bytes = cache
            .sources_read_bytes
            .saturating_add(stamp.map(|stamp| stamp.len).unwrap_or(0));
        crate::cost::counter("sources_read.calls", 1);
        Ok(Arc::clone(cache.sources.as_ref().expect("just stored")))
    }

    pub fn save_sources(&self, state: &SourcesFile) -> Result<(), ArchiveError> {
        {
            let mut cache = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.sources = Some(Arc::new(state.clone()));
            cache.sources_dirty = true;
        }
        self.flush_sources()
    }

    /// Write the staged source state, unless a capture-all round defers it.
    pub fn flush_sources(&self) -> Result<(), ArchiveError> {
        let mut cache = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !cache.sources_dirty || cache.deferred {
            return Ok(());
        }
        let sources = Arc::clone(cache.sources.as_ref().expect("dirty implies loaded"));
        let path = self.root.join("state").join("sources.json");
        let data = serde_json::to_string_pretty(sources.as_ref())
            .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        let _phase = crate::cost::phase("sources_write");
        atomic_write(&path, data.as_bytes())?;
        cache.sources_stamp = file_stamp(&path);
        cache.sources_dirty = false;
        cache.sources_flushes += 1;
        cache.sources_written_bytes = cache
            .sources_written_bytes
            .saturating_add(data.len() as u64);
        crate::cost::counter("sources_write.calls", 1);
        crate::cost::counter("sources_write.bytes", data.len() as u64);
        Ok(())
    }

    /// The record ledger (`state/records.json` + `state/records.jsonl`),
    /// loaded at most once per change.
    ///
    /// Read-only callers get the owned ledger (`Clone` of the cached map); the
    /// capture path uses [`Archive::records_contains`] /
    /// [`Archive::stage_record_ref`] / [`Archive::flush_records`] instead, so a
    /// round loads the ledger once and never clones it per file.
    pub fn load_records(&self) -> Result<RecordsFile, ArchiveError> {
        Ok((*self.records_snapshot()?).clone())
    }

    /// Canonical ledger path (`state/records.json`).
    fn records_canonical_path(&self) -> PathBuf {
        self.root
            .join("state")
            .join(records::RECORDS_CANONICAL_FILENAME)
    }

    /// Journal path (`state/records.jsonl`).
    fn records_tail_path(&self) -> PathBuf {
        self.root.join("state").join(records::RECORDS_TAIL_FILENAME)
    }

    fn records_snapshot(&self) -> Result<Arc<RecordsFile>, ArchiveError> {
        // §A: the record ledger is one of the unmeasured read paths; measured
        // inside the archive so every reader/writer call is covered once.
        let canonical = self.records_canonical_path();
        let tail = self.records_tail_path();
        let mut cache = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stamp = LedgerStamp::of(&canonical, &tail);
        if let Some(records) = cache.records.as_ref() {
            if cache.records_dirty || cache.records_stamp == Some(stamp) {
                return Ok(Arc::clone(records));
            }
        }
        let _phase = crate::cost::phase("records_read");
        let (ledger, stamp, read) = read_ledger(&self.root)?;
        crate::cost::counter("records_read.calls", 1);
        crate::cost::counter(
            "records_read.bytes",
            read.canonical_bytes.saturating_add(read.tail_bytes),
        );
        crate::cost::counter("records_tail.bytes", read.tail_bytes);
        crate::cost::counter("records_tail.lines", read.tail_lines);
        crate::cost::counter(
            "records_tail.incomplete_lines",
            read.tail_incomplete_lines,
        );
        crate::cost::counter("records_tail.duplicate_lines", read.tail_duplicate_lines);
        cache.records = Some(Arc::new(ledger));
        cache.records_stamp = Some(stamp);
        cache.records_loads += 1;
        cache.records_read_bytes = cache
            .records_read_bytes
            .saturating_add(read.canonical_bytes.saturating_add(read.tail_bytes));
        cache.records_tail_bytes = cache.records_tail_bytes.saturating_add(read.tail_bytes);
        cache.records_tail_incomplete_lines = cache
            .records_tail_incomplete_lines
            .saturating_add(read.tail_incomplete_lines);
        cache.records_tail_duplicate_lines = cache
            .records_tail_duplicate_lines
            .saturating_add(read.tail_duplicate_lines);
        Ok(Arc::clone(cache.records.as_ref().expect("just stored")))
    }

    /// Is this record sha already in the ledger (committed or staged this run)?
    pub fn records_contains(&self, record_sha: &str) -> Result<bool, ArchiveError> {
        Ok(self.records_snapshot()?.records.contains_key(record_sha))
    }

    /// The stored location of one record, resolved without cloning the ledger.
    pub fn record_ref(&self, record_sha: &str) -> Result<RecordRef, ArchiveError> {
        self.records_snapshot()?
            .records
            .get(record_sha)
            .cloned()
            .ok_or_else(|| ArchiveError::NotFound(format!("record {record_sha}")))
    }

    /// Stage one record reference. Returns `true` when the sha was new (the
    /// first-writer mapping wins, exactly like the previous `entry().or_insert`).
    /// The ledger itself is written by [`Archive::flush_records`]; only the new
    /// entries are appended by that write.
    pub fn stage_record_ref(
        &self,
        record_sha: &str,
        reference: RecordRef,
    ) -> Result<bool, ArchiveError> {
        self.records_snapshot()?;
        let mut cache = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let records = cache.records.as_mut().expect("loaded above");
        if records.records.contains_key(record_sha) {
            // First writer wins: a re-captured record never rewrites its mapping.
            return Ok(false);
        }
        Arc::make_mut(records)
            .records
            .insert(record_sha.to_string(), reference.clone());
        cache
            .records_staged
            .push((record_sha.to_string(), reference));
        cache.records_dirty = true;
        Ok(true)
    }

    /// Write the ledger if this archive staged changes. Inside a capture-all
    /// round the write is deferred to [`CaptureSession::finish`].
    ///
    /// A regular flush **appends** the staged entries to the journal
    /// (`state/records.jsonl`); the canonical file is only rewritten by a
    /// compaction (see [`Archive::compact_records`]). The write is one
    /// `write_all` of a complete lines block followed by `flush` + `sync_all`,
    /// so a crash can only leave a prefix of the last line, never a hole.
    pub fn flush_records(&self) -> Result<(), ArchiveError> {
        self.flush_records_inner(true)
    }

    fn flush_records_inner(&self, auto_compact: bool) -> Result<(), ArchiveError> {
        let mut cache = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !cache.records_dirty || cache.deferred {
            return Ok(());
        }
        if cache.records_replace {
            let records = Arc::clone(cache.records.as_ref().expect("dirty implies loaded"));
            drop(cache);
            // The flags stay set until the write succeeds, so a failed
            // replacement is retried by the next flush instead of vanishing.
            self.write_replaced_ledger(&records)?;
            let mut cache = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.records_replace = false;
            cache.records_dirty = false;
            cache.records_staged.clear();
            return Ok(());
        }
        let canonical_path = self.records_canonical_path();
        let tail_path = self.records_tail_path();
        let mut staged = std::mem::take(&mut cache.records_staged);
        if staged.is_empty() {
            cache.records_dirty = false;
            return Ok(());
        }
        // A concurrent writer (another process that ignores the single-writer
        // lock, or a CLI compaction) can move either stamp. When it did, our
        // in-memory view may be stale: reload, keep the on-disk mapping for any
        // sha the disk already holds (first writer wins) and append only what is
        // still missing. The common single-writer case costs two `stat`s.
        // The journal's age is measured **before** this round's append (after it,
        // the mtime is always "now"), so an idle journal is compacted by the next
        // round that appends to it.
        let tail_was_stale = file_stamp(&tail_path)
            .map(|stamp| now_ms().saturating_sub(stamp.mtime_ms) >= RECORDS_TAIL_COMPACT_MAX_AGE_MS)
            .unwrap_or(false);
        let stamp_now = LedgerStamp::of(&canonical_path, &tail_path);
        if cache.records_stamp != Some(stamp_now) {
            let (mut fresh, stamp, _read) = read_ledger(&self.root)?;
            let mut keep: Vec<(String, RecordRef)> = Vec::with_capacity(staged.len());
            for (sha, reference) in staged {
                if fresh.records.contains_key(&sha) {
                    continue;
                }
                fresh.records.insert(sha.clone(), reference.clone());
                keep.push((sha, reference));
            }
            crate::cost::counter("records_tail.stale_reloads", 1);
            cache.records = Some(Arc::new(fresh));
            cache.records_stamp = Some(stamp);
            staged = keep;
            if staged.is_empty() {
                cache.records_dirty = false;
                return Ok(());
            }
        }
        // First-writer mapping: for a record that is already committed, the
        // ledger stores the **earliest** committed mapping, not this capture's.
        // Without it, a record re-captured after a crashed round would be
        // recorded with the new capture id/object and the ledger would no longer
        // equal "replay the manifests in capture order, first writer wins".
        // One manifest listing per flush; the record index is built once per
        // process, on the first flush that stages records.
        let mut resolved_mappings = 0u64;
        {
            let snapshot = self.manifests()?;
            for (sha, reference) in staged.iter_mut() {
                let Some(committed) = snapshot.earliest_record_ref(sha) else {
                    continue;
                };
                if committed.object == reference.object
                    && committed.offset == reference.offset
                    && committed.len == reference.len
                    && committed.capture_id == reference.capture_id
                {
                    continue;
                }
                *reference = committed;
                resolved_mappings += 1;
            }
        }
        if resolved_mappings > 0 {
            crate::cost::counter("records_first_writer.resolved", resolved_mappings);
            cache.records_first_writer_resolved = cache
                .records_first_writer_resolved
                .saturating_add(resolved_mappings);
            if let Some(records) = cache.records.as_mut() {
                for (sha, reference) in &staged {
                    Arc::make_mut(records)
                        .records
                        .insert(sha.clone(), reference.clone());
                }
            }
        }
        let mut data: Vec<u8> = Vec::with_capacity(staged.len() * 192);
        for (sha, reference) in &staged {
            match records::journal_line(sha, reference) {
                Ok(line) => data.extend_from_slice(line.as_bytes()),
                Err(error) => {
                    cache.records_staged = staged;
                    return Err(error);
                }
            }
        }
        let _phase = crate::cost::phase("records_write");
        // The journal lock serialises this append (and the half-line repair
        // below) with a compaction's stamp check + removal. Without it a line
        // written between those two steps would be deleted before it was ever
        // merged — the one way an append inside the ledger's own protocol could
        // lose an entry. The wait is bounded so a lock file left behind by a
        // hard kill can never stall a capture round: past the budget the flush
        // takes the degraded path below, which does not write the journal.
        let journal = if cache.records_tail_lock_timeout && self.records_tail_lock_file().exists() {
            // Already known to be stuck: do not pay the wait again.
            None
        } else {
            cache.records_tail_lock_timeout = false;
            match self.acquire_lock_wait(RECORDS_TAIL_LOCK, RECORDS_TAIL_LOCK_WAIT_MS) {
                Ok(guard) => Some(guard),
                Err(ArchiveError::LockHeld(_)) => {
                    cache.records_tail_lock_timeout = true;
                    crate::cost::counter("records_tail.lock_timeout", 1);
                    None
                }
                Err(error) => {
                    cache.records_staged = staged;
                    return Err(error);
                }
            }
        };
        // Degraded path when the journal lock could not be taken.
        let journal = match journal {
            Some(guard) => guard,
            None => {
                // Degraded path: no journal lock. Never write the journal here —
                // a line written without the lock can land between a
                // compaction's "the journal is still the one I merged" check and
                // its `remove_file`, and is deleted before anything merged it.
                // The staged entries are promoted into the canonical instead
                // (see `promote_staged_records`). A promotion is a full canonical
                // rewrite, so no compaction is run afterwards: the canonical is
                // already the merged ledger.
                return match self.promote_staged_records(&mut cache, &staged) {
                    Ok(()) => {
                        crate::cost::counter("records_tail.promoted", 1);
                        drop(cache);
                        Ok(())
                    }
                    Err(error) => {
                        // Nothing was written: keep the entries staged and let
                        // the caller fail the round (the source watermark stays
                        // put, so the next round re-captures and re-stages).
                        cache.records_staged = staged;
                        Err(error)
                    }
                };
            }
        };
        // A crash can leave a prefix of one line at the end of the journal;
        // appending after it would glue the next entry onto a half line and make
        // every later read fail-closed. Drop it first (the record itself is
        // re-captured by the next round, its watermark did not advance).
        match records::repair_incomplete_tail(&tail_path) {
            Ok(repaired) => {
                if repaired > 0 {
                    crate::cost::counter("records_tail.repaired_bytes", repaired);
                }
            }
            Err(error) => {
                cache.records_staged = staged;
                return Err(error);
            }
        }
        if let Err(error) = records::append_tail(&tail_path, &data) {
            cache.records_staged = staged;
            return Err(error);
        }
        // The stamp is taken while the journal lock is still held, so no append
        // can land between this append and the sample: the bytes it describes
        // are on disk. Lines another writer appended between this instance's
        // last load and the lock are on disk and in the stamp, but not in this
        // cache — any later stamp move re-reads them, and the read side never
        // loses or reorders them.
        let stamp_after = LedgerStamp::of(&canonical_path, &tail_path);
        drop(journal);
        let appended = data.len() as u64;
        let entries = staged.len() as u64;
        cache.records_stamp = Some(stamp_after);
        cache.records_dirty = false;
        cache.records_flushes += 1;
        cache.records_appends += 1;
        cache.records_written_bytes = cache.records_written_bytes.saturating_add(appended);
        cache.records_append_bytes = cache.records_append_bytes.saturating_add(appended);
        crate::cost::counter("records_write.calls", 1);
        crate::cost::counter("records_write.bytes", appended);
        crate::cost::counter("records_write.entries", entries);
        drop(cache);
        if auto_compact {
            self.maybe_compact_records(tail_was_stale)?;
        }
        Ok(())
    }

    /// Replace the whole ledger (single-source calls that own the file): write
    /// the canonical file and then drop the journal, so the replacement really
    /// is the whole ledger and no journal line can re-appear through the merge.
    pub fn save_records(&self, records: &RecordsFile) -> Result<(), ArchiveError> {
        {
            let mut cache = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.records = Some(Arc::new(records.clone()));
            cache.records_staged.clear();
            cache.records_dirty = true;
            cache.records_replace = true;
        }
        self.flush_records_inner(false)
    }

    /// Canonical write for a whole-ledger replacement (see
    /// [`Archive::save_records`]): canonical first, journal second, like a
    /// compaction — the reverse order could lose journal entries.
    ///
    /// Both ledger locks are taken (compaction lock first, journal lock second,
    /// the same order a compaction uses): the replacement must not interleave
    /// with an append, and it must not run while another rewrite is in flight.
    /// A journal lock that is still held after the budget fails the replacement
    /// (`LockHeld`) instead of leaving a journal that would resurrect the entries
    /// this call was asked to drop — "keep the journal, trim next round" is the
    /// compaction's degraded path and is not available to a replacement. A lock
    /// that only *looked* held — Windows reports a `create_new` on a file that is
    /// being deleted as `ERROR_ACCESS_DENIED` — gets one more bounded round
    /// first, so transient contention completes instead of failing.
    fn write_replaced_ledger(&self, records: &RecordsFile) -> Result<(), ArchiveError> {
        let canonical = self.records_canonical_path();
        let tail = self.records_tail_path();
        let data = serde_json::to_string_pretty(records)
            .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        let _lock = self.acquire_named_lock(RECORDS_COMPACT_LOCK)?;
        let _phase = crate::cost::phase("records_compact");
        let stamp_after = {
            let _journal =
                match self.acquire_lock_wait(RECORDS_TAIL_LOCK, RECORDS_TAIL_LOCK_WAIT_MS) {
                    Ok(guard) => guard,
                    Err(error) if lock_contended(&error) => {
                        crate::cost::counter("records_compact.replace_lock_retry", 1);
                        match self.acquire_lock_wait(RECORDS_TAIL_LOCK, RECORDS_TAIL_LOCK_WAIT_MS) {
                            Ok(guard) => guard,
                            // Still contended after a second budget: report the
                            // class the caller can act on (a lock, not a broken
                            // disk) and write nothing.
                            Err(second) if lock_contended(&second) => {
                                return Err(ArchiveError::LockHeld(self.records_tail_lock_file()));
                            }
                            Err(second) => return Err(second),
                        }
                    }
                    Err(error) => return Err(error),
                };
            atomic_write(&canonical, data.as_bytes())?;
            records::remove_tail(&tail)?;
            LedgerStamp::of(&canonical, &tail)
        };
        let mut cache = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.records_stamp = Some(stamp_after);
        cache.records_dirty = false;
        cache.records_compactions += 1;
        cache.records_compact_bytes = cache
            .records_compact_bytes
            .saturating_add(data.len() as u64);
        crate::cost::counter("records_compact.calls", 1);
        crate::cost::counter("records_compact.bytes", data.len() as u64);
        crate::cost::counter("records_compact.replaces", 1);
        Ok(())
    }

    /// Compact the record ledger when it is due: rewrite `state/records.json`
    /// from the **merged** ledger (canonical + journal) and then remove the
    /// journal. Due means the journal exceeds
    /// [`RECORDS_TAIL_COMPACT_BYTES`], has not been appended to for
    /// [`RECORDS_TAIL_COMPACT_MAX_AGE_MS`] (measured from its last append
    /// before this round), or the canonical file does not exist yet (a fresh
    /// root gets its canonical on the first flush).
    ///
    /// An automatic compaction never fails a round: the journal is already
    /// durable, so the evidence is complete. A failure is counted
    /// (`records_compact.errors`) and its message kept for
    /// [`Archive::records_compact_error`]. A compaction that cannot take
    /// `locks/records-compact.lock` is skipped and counted
    /// (`records_compact.skipped_locked`).
    fn maybe_compact_records(&self, tail_was_stale: bool) -> Result<(), ArchiveError> {
        let canonical = self.records_canonical_path();
        let tail = self.records_tail_path();
        let Some(tail_stamp) = file_stamp(&tail) else {
            return Ok(());
        };
        let due = !canonical.exists()
            || tail_stamp.len > RECORDS_TAIL_COMPACT_BYTES
            || tail_was_stale;
        if !due {
            return Ok(());
        }
        // The compaction lock covers the **whole** run (merged read, canonical
        // rewrite, journal removal), not just the acquisition: taking it and
        // dropping the guard at the end of the match arm would let a second
        // automatic compaction interleave two rewrites, and one of them could
        // overwrite the other's newer canonical. `compact_guard` therefore lives
        // until the run below returns (a mutation of that lifetime is what the
        // `an_automatic_compaction_holds_the_compaction_lock_for_its_whole_run`
        // unit test pins).
        let mut compact_guard = Some(match self.acquire_named_lock(RECORDS_COMPACT_LOCK) {
            Ok(guard) => guard,
            Err(ArchiveError::LockHeld(_)) => {
                crate::cost::counter("records_compact.skipped_locked", 1);
                return Ok(());
            }
            Err(error) => return Err(error),
        });
        let outcome = self.compact_records_locked();
        drop(compact_guard.take());
        match outcome {
            Ok(report) => {
                crate::cost::counter("records_compact.auto", u64::from(report.performed));
                Ok(())
            }
            Err(error) => {
                crate::cost::counter("records_compact.errors", 1);
                let mut cache = self
                    .ledger
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.records_compact_errors = cache.records_compact_errors.saturating_add(1);
                cache.records_compact_error = Some(error.to_string());
                Ok(())
            }
        }
    }

    /// The merged ledger of **both** on-disk forms, read fresh from disk.
    /// Returns the ledger, the stamps it was read from, and the read statistics.
    fn read_ledger_fresh(&self) -> Result<(RecordsFile, LedgerStamp, LedgerRead), ArchiveError> {
        read_ledger(&self.root)
    }

    /// Compact now (see [`Archive::maybe_compact_records`] for the trigger).
    ///
    /// `dry_run` reports what a compaction would do and writes nothing. A real
    /// run holds `locks/records-compact.lock`; a concurrent compaction gets
    /// [`ArchiveError::LockHeld`] instead of interleaving two rewrites. After
    /// taking the lock it re-reads the ledger from disk, so entries appended by
    /// another process before the lock are included, and it removes the journal
    /// only while holding `locks/records-tail.lock` **and** only when the journal
    /// is still byte-for-byte the one it merged. An append takes the same journal
    /// lock around its own write, so no line can slip in between the stamp check
    /// and the removal: the journal is either removed before that append (which
    /// then starts a fresh one) or kept.
    pub fn compact_records(&self, dry_run: bool) -> Result<CompactRecordsReport, ArchiveError> {
        if dry_run {
            let (ledger, _stamp, read) = self.read_ledger_fresh()?;
            return Ok(CompactRecordsReport {
                records_before: read.canonical_records,
                records_after: ledger.records.len() as u64,
                tail_bytes: read.tail_bytes,
                tail_lines: read.tail_lines,
                tail_incomplete_lines: read.tail_incomplete_lines,
                tail_duplicate_lines: read.tail_duplicate_lines,
                bytes_written: 0,
                performed: false,
                skipped: Some("dry_run".to_string()),
                tail_removed: false,
                tail_kept_because_changed: false,
                tail_kept_because_locked: false,
                canonical_bytes: read.canonical_bytes,
            });
        }
        // Stage → append first (append only, no recursive compaction), so a
        // pending round's entries are part of the merged ledger below.
        self.flush_records_inner(false)?;
        let _lock = self.acquire_named_lock(RECORDS_COMPACT_LOCK)?;
        self.compact_records_locked()
    }

    fn compact_records_locked(&self) -> Result<CompactRecordsReport, ArchiveError> {
        {
            let cache = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cache.deferred {
                return Ok(CompactRecordsReport {
                    records_before: 0,
                    records_after: 0,
                    tail_bytes: 0,
                    tail_lines: 0,
                    tail_incomplete_lines: 0,
                    tail_duplicate_lines: 0,
                    bytes_written: 0,
                    performed: false,
                    skipped: Some("capture_session".to_string()),
                    tail_removed: false,
                    tail_kept_because_changed: false,
                    tail_kept_because_locked: false,
                    canonical_bytes: 0,
                });
            }
        }
        let canonical = self.records_canonical_path();
        let tail = self.records_tail_path();
        let (mut ledger, stamp, read) = self.read_ledger_fresh()?;
        crate::cost::counter("records_compact.input", 1);
        crate::cost::counter("records_compact.input_tail_bytes", read.tail_bytes);
        if read.tail_bytes == 0 && read.canonical_bytes > 0 {
            return Ok(CompactRecordsReport {
                records_before: read.canonical_records,
                records_after: ledger.records.len() as u64,
                tail_bytes: 0,
                tail_lines: 0,
                tail_incomplete_lines: 0,
                tail_duplicate_lines: 0,
                bytes_written: 0,
                performed: false,
                skipped: Some("nothing_to_compact".to_string()),
                tail_removed: false,
                tail_kept_because_changed: false,
                tail_kept_because_locked: false,
                canonical_bytes: read.canonical_bytes,
            });
        }
        if ledger.schema.is_empty() {
            ledger.schema = RECORDS_SCHEMA.to_string();
        }
        let records_after = ledger.records.len() as u64;
        let data =
            serde_json::to_string_pretty(&ledger).map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        let _phase = crate::cost::phase("records_compact");
        // Canonical first. Only once it is on disk may the journal go away.
        atomic_write(&canonical, data.as_bytes())?;
        // The removal is the one step that can lose an entry, so it takes the
        // journal lock: an append holds that lock across its repair + write, so
        // no line can appear between the stamp comparison and the `remove_file`
        // below. The guard is held for exactly one `stat`, one read of the whole
        // journal (≤ `RECORDS_TAIL_COMPACT_BYTES` ≈ 1 MiB, for the content check)
        // and one `remove_file` (`RECORDS_TAIL_TRIM_WAIT_MS` at most), never for
        // the canonical rewrite above — a capture round therefore never waits
        // for a compaction, only for the other appender's own `fsync`.
        let mut tail_removed = false;
        let mut tail_kept_because_changed = false;
        let mut tail_kept_because_locked = false;
        match self.acquire_lock_wait(RECORDS_TAIL_LOCK, RECORDS_TAIL_TRIM_WAIT_MS) {
            Ok(_journal) => {
                let tail_after = file_stamp(&tail);
                // Two checks, because the stamp alone cannot tell two journals
                // apart: a journal that was truncated (half-line repair) and
                // refilled to the same length in the same millisecond has the
                // same stamp. The digest is over the exact bytes merged above, so
                // "unchanged" is a content claim and not a size claim.
                let unchanged = tail_after == stamp.tail
                    && self.journal_is_the_one_merged(&tail, &read)?;
                if unchanged {
                    // Test-only interleaving point: the exact window between the
                    // stamp check and the removal.
                    #[cfg(test)]
                    ledger_probe::enter(ledger_probe::Point::CompactionTrimWindow, &self.root);
                    records::remove_tail(&tail)?;
                    tail_removed = true;
                } else {
                    // A writer appended between our read and now. The canonical
                    // we just wrote covers everything we read; the journal stays,
                    // so the newer lines stay visible through the merge.
                    tail_kept_because_changed = true;
                    crate::cost::counter("records_compact.tail_kept", 1);
                }
            }
            Err(ArchiveError::LockHeld(_)) => {
                // A lock file left behind by a hard kill (or an appender that has
                // held it for longer than any append takes). Keeping the journal
                // is always safe: the canonical already holds everything that was
                // merged, so the journal is only duplicates.
                tail_kept_because_locked = true;
                crate::cost::counter("records_compact.tail_kept_locked", 1);
            }
            Err(error) if lock_contended(&error) => {
                // The budget ran out while the lock file was being deleted
                // (Windows `ERROR_ACCESS_DENIED`, see `lock_contended`). Same
                // decision as a held lock: the canonical is already written, the
                // journal is only duplicates, so keep it and trim it next round
                // instead of failing a compaction that has nothing left to lose.
                tail_kept_because_locked = true;
                crate::cost::counter("records_compact.tail_kept_locked", 1);
                crate::cost::counter("records_compact.tail_kept_denied", 1);
            }
            Err(error) => return Err(error),
        }
        {
            let mut cache = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.records = Some(Arc::new(ledger));
            cache.records_staged.clear();
            cache.records_dirty = false;
            cache.records_stamp = Some(LedgerStamp::of(&canonical, &tail));
            cache.records_compactions += 1;
            cache.records_compact_bytes = cache
                .records_compact_bytes
                .saturating_add(data.len() as u64);
            crate::cost::counter("records_compact.calls", 1);
            crate::cost::counter("records_compact.bytes", data.len() as u64);
            crate::cost::counter("records_compact.records", records_after);
            crate::cost::counter("records_compact.tail_bytes", read.tail_bytes);
        }
        Ok(CompactRecordsReport {
            records_before: read.canonical_records,
            records_after,
            tail_bytes: read.tail_bytes,
            tail_lines: read.tail_lines,
            tail_incomplete_lines: read.tail_incomplete_lines,
            tail_duplicate_lines: read.tail_duplicate_lines,
            bytes_written: data.len() as u64,
            performed: true,
            skipped: None,
            tail_removed,
            tail_kept_because_changed,
            tail_kept_because_locked,
            canonical_bytes: data.len() as u64,
        })
    }

    /// Last automatic compaction failure message (see
    /// [`Archive::maybe_compact_records`]).
    pub fn records_compact_error(&self) -> Option<String> {
        self.ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records_compact_error
            .clone()
    }

    /// Ledger load/write counters of this archive instance.
    pub fn ledger_stats(&self) -> LedgerStats {
        let cache = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        LedgerStats {
            records_loads: cache.records_loads,
            records_flushes: cache.records_flushes,
            records_read_bytes: cache.records_read_bytes,
            records_written_bytes: cache.records_written_bytes,
            records_appends: cache.records_appends,
            records_append_bytes: cache.records_append_bytes,
            records_compactions: cache.records_compactions,
            records_compact_bytes: cache.records_compact_bytes,
            records_promotions: cache.records_promotions,
            records_tail_bytes: cache.records_tail_bytes,
            records_tail_incomplete_lines: cache.records_tail_incomplete_lines,
            records_tail_duplicate_lines: cache.records_tail_duplicate_lines,
            records_compact_errors: cache.records_compact_errors,
            records_first_writer_resolved: cache.records_first_writer_resolved,
            sources_loads: cache.sources_loads,
            sources_flushes: cache.sources_flushes,
            sources_read_bytes: cache.sources_read_bytes,
            sources_written_bytes: cache.sources_written_bytes,
            deferred: cache.deferred,
        }
    }

    // -- reconciliation --------------------------------------------------------
    /// Compare objects on disk against manifest references. Orphans are
    /// reported, never deleted. `pending` lists objects written by the current
    /// in-flight capture (not yet referenced) so reconciliation under the
    /// writer's view does not flag them. Includes the per-encoding storage
    /// statistics and any dual-encoding crash-window leftovers.
    pub fn reconcile(&self, pending: &[String]) -> Result<ReconcileReport, ArchiveError> {
        // §A: object-directory walk + full manifest load; measured as its own
        // stage so a capture run's unaccounted reads become visible.
        let _phase = crate::cost::phase("reconcile");
        let objects_dir = self.root.join("archive").join("objects");
        let mut on_disk = BTreeSet::new();
        let mut plain_shas = BTreeSet::new();
        let mut zstd_shas = BTreeSet::new();
        for tier1 in read_dir_sorted(&objects_dir)? {
            if !tier1.path().is_dir() {
                continue;
            }
            for tier2 in read_dir_sorted(&tier1.path())? {
                if !tier2.path().is_dir() {
                    continue;
                }
                for obj in read_dir_sorted(&tier2.path())? {
                    let name = obj.file_name().to_string_lossy().to_string();
                    match name.strip_suffix(ZSTD_OBJECT_SUFFIX) {
                        Some(sha) => {
                            zstd_shas.insert(sha.to_string());
                            on_disk.insert(sha.to_string());
                        }
                        None => {
                            plain_shas.insert(name.clone());
                            on_disk.insert(name);
                        }
                    }
                }
            }
        }
        let mut referenced = BTreeSet::new();
        let manifests = self.manifests()?;
        for manifest in manifests.iter() {
            for obj in &manifest.objects {
                referenced.insert(obj.sha256.clone());
            }
        }
        let pending_set: BTreeSet<&str> = pending.iter().map(String::as_str).collect();
        let orphan_objects: Vec<String> = on_disk
            .difference(&referenced)
            .filter(|sha| !pending_set.contains(sha.as_str()))
            .cloned()
            .collect();
        let missing_objects: Vec<String> = referenced
            .difference(&on_disk)
            .cloned()
            .collect();
        let dual_encoding: Vec<String> = plain_shas
            .intersection(&zstd_shas)
            .cloned()
            .collect();
        let mut staging_residue = Vec::new();
        for entry in read_dir_sorted(&self.root.join("archive").join("staging"))? {
            staging_residue.push(entry.file_name().to_string_lossy().to_string());
        }
        let storage = self.storage_stats()?;
        Ok(ReconcileReport {
            objects_on_disk: on_disk.len() as u64,
            objects_referenced: referenced.len() as u64,
            manifests: manifests.len() as u64,
            orphan_objects,
            missing_objects,
            staging_residue,
            storage,
            dual_encoding,
        })
    }

    /// Post-capture-unit reconcile, unless a capture-all round is open.
    ///
    /// One object-directory walk plus one manifest pass is a whole-store cost:
    /// doing it per captured file made a 36-file round walk the store 36 times
    /// (0.44 GB / 13–20 s each). Inside a round the caller reconciles once after
    /// every source finished, so the per-unit call reports `None` instead of a
    /// fake empty result.
    pub fn reconcile_after_unit(&self) -> Result<Option<ReconcileReport>, ArchiveError> {
        if self.capture_session_active() {
            return Ok(None);
        }
        Ok(Some(self.reconcile(&[])?))
    }

    fn capture_session_active(&self) -> bool {
        self.ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .deferred
    }

    /// Open a capture-all round: the record ledger and the source state are
    /// staged in memory and written once at [`CaptureSession::finish`], and
    /// per-unit reconciles are deferred to the round end.
    ///
    /// Write order at flush is records → sources, the same order as the
    /// per-unit path: a crash between the two leaves ledger entries that no
    /// watermark claims, which the next run re-captures idempotently (the
    /// existing `after_manifest_before_state` recovery). A crash before the
    /// flush leaves committed manifests with no ledger entry and no advanced
    /// watermark, so the next run re-captures the same bytes instead of
    /// skipping them.
    pub fn begin_capture_session(&self) -> CaptureSession<'_> {
        {
            let mut cache = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.deferred = true;
        }
        CaptureSession { archive: self }
    }

    // -- readback (fail-closed) ----------------------------------------------

    /// Open a readback handle bound to one access domain. Handles are never
    /// shared across domains; every read re-evaluates access from the committed
    /// manifest snapshot (refreshed from the directory listing, never re-parsed
    /// per read).
    pub fn reader(&self, domain: &str) -> ReadHandle<'_> {
        ReadHandle {
            archive: self,
            domain: domain.to_string(),
        }
    }

    /// Access decision for one object sha: allowed iff `domain` appears in the
    /// access tags of EVERY committed manifest reference to it (strictest
    /// intersection). Unknown or unmapped (empty tags) references deny.
    fn object_accessible(&self, sha256: &str, domain: &str) -> Result<(), ArchiveError> {
        self.manifests()?.check_object(sha256, domain)
    }

    /// Access decision for one record; `Ok(true)` when the record's carrier
    /// object is an application-level compressed SQLite batch whose manifest
    /// offsets address the decompressed payload, `Ok(false)` when the decoded
    /// object bytes can be sliced directly (every non-SQLite capture).
    fn record_accessible(
        &self,
        record_sha: &str,
        rec: &RecordRef,
        domain: &str,
    ) -> Result<bool, ArchiveError> {
        self.manifests()?.check_record(record_sha, rec, domain)
    }
}

pub struct ReadHandle<'a> {
    archive: &'a Archive,
    domain: String,
}

impl<'a> ReadHandle<'a> {
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Read a whole object by sha256 with access check + hash verification.
    pub fn read_object(&self, sha256: &str) -> Result<Vec<u8>, ArchiveError> {
        self.archive.object_accessible(sha256, &self.domain)?;
        self.archive.read_object_bytes(sha256)
    }

    /// Read one captured source record (raw source bytes) by record sha256.
    ///
    /// SQLite evidence stores each delta batch as one application-level zstd
    /// payload whose object is kept verbatim (`StorageHint::Raw`), while the
    /// manifest record offsets address the **decompressed** JSONL batch. For
    /// such records (N5, stage-2 S5) the payload is decoded before slicing;
    /// every other record slices the decoded object bytes directly. The final
    /// hash verification is unchanged and still fail-closed.
    pub fn read_record(&self, record_sha: &str) -> Result<Vec<u8>, ArchiveError> {
        // One ledger lookup (the ledger is loaded at most once per change), one
        // access decision against the shared manifest snapshot.
        let rec = self.archive.record_ref(record_sha)?;
        let batch_compressed = self.archive.record_accessible(record_sha, &rec, &self.domain)?;
        let object = self.archive.read_object_bytes(&rec.object)?;
        let payload = if batch_compressed {
            zstd::stream::decode_all(object.as_slice()).map_err(|e| {
                ArchiveError::Integrity(format!(
                    "record {record_sha} batch object {} is not valid zstd: {e}",
                    rec.object
                ))
            })?
        } else {
            object
        };
        let start = usize::try_from(rec.offset)
            .map_err(|_| ArchiveError::Corrupt("record offset overflows".into()))?;
        let len = usize::try_from(rec.len)
            .map_err(|_| ArchiveError::Corrupt("record len overflows".into()))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| ArchiveError::Corrupt("record span overflows".into()))?;
        if end > payload.len() {
            return Err(ArchiveError::Corrupt(format!(
                "record {record_sha} span exceeds object {}",
                rec.object
            )));
        }
        let slice = &payload[start..end];
        let actual = hex_sha256(slice);
        if actual != record_sha {
            return Err(ArchiveError::Integrity(format!(
                "record {record_sha} slice hashes to {actual}"
            )));
        }
        Ok(slice.to_vec())
    }

    /// P4: read the archived bytes behind one ContentRef mapping.
    ///
    /// Fail-closed on every axis: an unknown capture/sequence is `NotFound`, a
    /// non-archived entry (missing / rejected) is `NotFound` with its status, an
    /// empty or non-matching reference access mapping is `AccessDenied`, and the
    /// shared object is additionally checked against the strictest intersection
    /// of ALL its committed references before any byte is returned.
    pub fn read_attachment(&self, capture_id: &str, seq: u64) -> Result<Vec<u8>, ArchiveError> {
        if self.domain.is_empty() {
            return Err(ArchiveError::AccessDenied(
                "empty access domain (fail-closed)".to_string(),
            ));
        }
        let (_, entry) = self
            .archive
            .find_attachment(capture_id, seq)?
            .ok_or_else(|| {
                ArchiveError::NotFound(format!("attachment {}", attachment_id(capture_id, seq)))
            })?;
        if entry.status != ATTACHMENT_STATUS_ARCHIVED {
            return Err(ArchiveError::NotFound(format!(
                "attachment {} is not archived (status={})",
                attachment_id(capture_id, seq),
                entry.status
            )));
        }
        let sha = entry.object_sha256.as_deref().ok_or_else(|| {
            ArchiveError::NotFound(format!(
                "attachment {} is archived without a content address",
                attachment_id(capture_id, seq)
            ))
        })?;
        if entry.access.is_empty() {
            return Err(ArchiveError::AccessDenied(format!(
                "attachment {} has no access mapping (fail-closed)",
                attachment_id(capture_id, seq)
            )));
        }
        if !entry.access.iter().any(|d| d == &self.domain) {
            return Err(ArchiveError::AccessDenied(format!(
                "attachment {} is not visible to domain {:?}",
                attachment_id(capture_id, seq),
                self.domain
            )));
        }
        self.archive.object_accessible(sha, &self.domain)?;
        self.archive.read_object_bytes(sha)
    }
}

/// One capture-all round. While it lives, the record ledger and the source
/// state are staged in memory and the per-unit reconcile is deferred; the
/// single write happens at [`CaptureSession::finish`].
///
/// Dropping a session without `finish` (an early return or a panic) still
/// best-effort flushes the staged ledgers, so a committed manifest never loses
/// its record entries on an error path. The returned error is ignored on drop
/// because the surrounding command already carries the real failure.
pub struct CaptureSession<'a> {
    archive: &'a Archive,
}

impl CaptureSession<'_> {
    pub fn archive(&self) -> &Archive {
        self.archive
    }

    /// Write the staged ledgers (records first, then source state) and close
    /// the round.
    pub fn finish(self) -> Result<(), ArchiveError> {
        let result = self.flush();
        // The drop flush must not run a second time after an explicit finish.
        std::mem::forget(self);
        result
    }

    fn flush(&self) -> Result<(), ArchiveError> {
        {
            let mut cache = self
                .archive
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.deferred = false;
        }
        self.archive.flush_records()?;
        self.archive.flush_sources()?;
        Ok(())
    }
}

impl Drop for CaptureSession<'_> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// object encoding helpers (stage-2 S2.5)
// ---------------------------------------------------------------------------

/// Worth-compression test: the encoded form must be strictly smaller than
/// [`ZSTD_WORTH_PERCENT`] percent of the raw size. Already-compressed content
/// fails this and keeps the plain encoding.
pub fn worth_compressing(raw_len: u64, compressed_len: u64) -> bool {
    compressed_len
        .saturating_mul(100)
        < raw_len.saturating_mul(ZSTD_WORTH_PERCENT)
}

/// Verify a staged zstd object file: stream-decode it from disk (memory bounded
/// by [`OBJECT_CHUNK_BYTES`]) and check that the decoded bytes hash to
/// `sha256` and total `expected_len`. Used before any atomic rename into the
/// object store, so a `.zst` object file that exists is always verified content.
pub(crate) fn verify_staged_zstd(
    staging: &Path,
    sha256: &str,
    expected_len: u64,
) -> Result<(), ArchiveError> {
    let file = fs::File::open(staging).map_err(|e| AppError::io(staging, e))?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|e| ArchiveError::Integrity(format!("object {sha256} is not valid zstd: {e}")))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; OBJECT_CHUNK_BYTES];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|e| ArchiveError::Integrity(format!("object {sha256} is not valid zstd: {e}")))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let actual = hex_lower(&hasher.finalize());
    if actual != sha256 || total != expected_len {
        return Err(ArchiveError::Integrity(format!(
            "compressed staging for {sha256} decoded to {total} bytes hashing to {actual}"
        )));
    }
    Ok(())
}

/// Decoded byte length of a zstd object file, streamed (memory bounded); used
/// only for objects no committed manifest references.
fn decoded_len_of(path: &Path) -> Result<u64, ArchiveError> {
    let file = fs::File::open(path).map_err(|e| AppError::io(path, e))?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|e| ArchiveError::Integrity(format!("{} is not valid zstd: {e}", path.display())))?;
    let mut total = 0u64;
    let mut buffer = vec![0u8; OBJECT_CHUNK_BYTES];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|e| AppError::io(path, e))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
    }
    Ok(total)
}

/// A reader that can be rewound to its start. [`Archive::
/// put_object_stream_encoded`] uses it to re-read the source when the
/// compressed encoding turns out not to be worth it.
pub trait RestartableRead: Read {
    fn reset(&mut self) -> io::Result<()>;
}

impl RestartableRead for fs::File {
    fn reset(&mut self) -> io::Result<()> {
        use std::io::Seek;
        self.seek(io::SeekFrom::Start(0))?;
        Ok(())
    }
}

impl<T: AsRef<[u8]>> RestartableRead for io::Cursor<T> {
    fn reset(&mut self) -> io::Result<()> {
        self.set_position(0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// Stable readback handle for one ContentRef mapping.
pub fn attachment_id(capture_id: &str, seq: u64) -> String {
    format!("{capture_id}#{seq}")
}

/// True for a lowercase 64-hex-digit content address. Everything else is
/// rejected before it can reach a filesystem path (no panic, no traversal).
fn is_valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn now_ms() -> i64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(ms).unwrap_or(i64::MAX)
}

pub fn new_id(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut seed = Vec::new();
    seed.extend_from_slice(prefix.as_bytes());
    seed.extend_from_slice(ts.to_string().as_bytes());
    seed.extend_from_slice(std::process::id().to_string().as_bytes());
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    seed.extend_from_slice(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string().as_bytes());
    format!("{}-{}", prefix, hex_sha256(&seed)[..24].to_string())
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<fs::DirEntry>, ArchiveError> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(AppError::io(dir, e).into()),
    };
    for entry in entries {
        out.push(entry.map_err(|e| AppError::io(dir, e))?);
    }
    out.sort_by_key(|e| e.file_name());
    Ok(out)
}

#[cfg(test)]
mod ledger_probe {
    //! Deterministic interleaving points for the journal's concurrency tests.
    //!
    //! Test builds only: `#[cfg(test)]` keeps the whole module (and its call
    //! sites) out of the library a dependent crate links and out of release
    //! binaries, so production code has no branch here. The hooks exist because
    //! the property under test is a *window* — "no append between the stamp
    //! check and the removal" — and a window that narrows to microseconds cannot
    //! be hit reliably by racing threads; a pause at the exact instruction turns
    //! the race into an ordered, deterministic test.

    use std::path::Path;
    use std::sync::{Arc, Mutex, OnceLock};

    /// Where a test may pause or inject work.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Point {
        /// After the canonical file was read and before the journal is read,
        /// inside one attempt of [`super::read_ledger_once`].
        ReadBetweenFiles,
        /// Inside the journal lock, after the journal stamp check and before the
        /// `remove_file` of a compaction.
        CompactionTrimWindow,
    }

    /// The hook receives the runtime root of the archive that reached the
    /// point: one process runs many archives (the whole lib test binary runs in
    /// parallel), so a hook must be able to ignore the others.
    type Hook = Arc<dyn Fn(Point, &Path) + Send + Sync + 'static>;

    fn slot() -> &'static Mutex<Option<Hook>> {
        static HOOK: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();
        HOOK.get_or_init(|| Mutex::new(None))
    }

    /// Install the hook that runs at every [`Point`] (replacing any previous one).
    pub(crate) fn set(hook: Hook) {
        if let Ok(mut slot) = slot().lock() {
            *slot = Some(hook);
        }
    }

    /// Remove the installed hook (also used by the tests' drop guard).
    pub(crate) fn clear() {
        if let Ok(mut slot) = slot().lock() {
            *slot = None;
        }
    }

    /// Run the installed hook, if any. The hook is cloned out before it runs: a
    /// hook may execute archive code that reaches another point.
    pub(crate) fn enter(point: Point, root: &Path) {
        let hook = slot().lock().ok().and_then(|slot| slot.clone());
        if let Some(hook) = hook {
            hook(point, root);
        }
    }
}

#[cfg(test)]
mod p4_object_stream_tests {
    use super::*;
    use std::io::{self, Read};

    /// Reader that hands out `step` bytes at a time, forcing many chunks.
    struct Dribble<'a> {
        data: &'a [u8],
        pos: usize,
        step: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let take = self
                .step
                .min(buf.len())
                .min(self.data.len() - self.pos);
            buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            Ok(take)
        }
    }

    impl RestartableRead for Dribble<'_> {
        fn reset(&mut self) -> io::Result<()> {
            self.pos = 0;
            Ok(())
        }
    }

    #[test]
    fn streamed_object_matches_in_memory_hash_across_chunks() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::init(temp.path()).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|index| (index % 251) as u8).collect();
        let mut reader = Dribble {
            data: &data,
            pos: 0,
            step: 7,
        };
        let (sha, bytes, is_new) = archive
            .put_object_stream(&mut reader, data.len() as u64)
            .unwrap();
        assert_eq!(sha, hex_sha256(&data));
        assert_eq!(bytes, data.len() as u64);
        assert!(is_new);
        assert_eq!(
            archive.verify_object_stream(&sha).unwrap(),
            data.len() as u64
        );
        assert_eq!(archive.read_object_bytes(&sha).unwrap(), data);

        // A second stream of identical bytes deduplicates to the same object.
        let mut again = Dribble {
            data: &data,
            pos: 0,
            step: 4096,
        };
        let (sha_again, _, is_new_again) = archive
            .put_object_stream(&mut again, data.len() as u64)
            .unwrap();
        assert_eq!(sha_again, sha);
        assert!(!is_new_again);
        assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);
    }

    #[test]
    fn stream_limit_aborts_without_committing_an_object() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::init(temp.path()).unwrap();
        let data = vec![7u8; 10_000];
        let mut reader = Dribble {
            data: &data,
            pos: 0,
            step: 1024,
        };
        let error = archive.put_object_stream(&mut reader, 4096).unwrap_err();
        assert!(matches!(error, ArchiveError::LimitExceeded(4096)));
        let reconcile = archive.reconcile(&[]).unwrap();
        assert_eq!(reconcile.objects_on_disk, 0);
        assert!(
            reconcile.staging_residue.is_empty(),
            "staging residue: {:?}",
            reconcile.staging_residue
        );
        assert!(!archive.has_object(&hex_sha256(&data)));
    }
}

#[cfg(test)]
mod s25_object_encoding_tests {
    use super::*;

    fn init() -> (tempfile::TempDir, Archive) {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::init(temp.path()).unwrap();
        (temp, archive)
    }

    #[test]
    fn compressible_bytes_take_the_zstd_encoding_and_round_trip() {
        let (_temp, archive) = init();
        // Repetitive JSONL-like content compresses far below the 95% threshold.
        let mut raw = Vec::new();
        for _ in 0..2000 {
            raw.extend_from_slice(b"{\"a\":1,\"b\":\"payload\"}\n");
        }
        let sha = hex_sha256(&raw);
        let (got_sha, got_len, is_new, encoding) = archive
            .put_object_encoded(&raw, StorageHint::Compressible)
            .unwrap();
        assert_eq!(got_sha, sha);
        assert_eq!(got_len, raw.len() as u64);
        assert!(is_new);
        assert_eq!(encoding, ObjectEncoding::Zstd);
        assert_eq!(archive.object_encoding(&sha), Some(ObjectEncoding::Zstd));
        assert!(archive.object_zstd_path(&sha).exists());
        assert!(!archive.object_path(&sha).exists());
        // Readback is byte-exact through the single decode point.
        assert_eq!(archive.read_object_bytes(&sha).unwrap(), raw);
        assert_eq!(archive.verify_object_stream(&sha).unwrap(), raw.len() as u64);
    }

    #[test]
    fn raw_hint_and_incompressible_content_stay_plain() {
        let (_temp, archive) = init();
        // Hint Raw: stored verbatim even though it would compress.
        let mut raw = Vec::new();
        for _ in 0..1000 {
            raw.extend_from_slice(b"{\"a\":1}\n");
        }
        let (sha, _, _, encoding) = archive
            .put_object_encoded(&raw, StorageHint::Raw)
            .unwrap();
        assert_eq!(encoding, ObjectEncoding::Plain);
        assert_eq!(archive.object_encoding(&sha), Some(ObjectEncoding::Plain));
        assert_eq!(archive.read_object_bytes(&sha).unwrap(), raw);

        // Pseudo-random (incompressible) content with a Compressible hint
        // fails the worth test and stays plain.
        let mut value: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut noise = Vec::with_capacity(40 * 1024);
        for _ in 0..40 * 1024 / 8 {
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            noise.extend_from_slice(&value.to_le_bytes());
        }
        let (sha2, _, _, encoding2) = archive
            .put_object_encoded(&noise, StorageHint::Compressible)
            .unwrap();
        assert_eq!(encoding2, ObjectEncoding::Plain);
        assert_eq!(archive.read_object_bytes(&sha2).unwrap(), noise);
    }

    #[test]
    fn content_starting_with_the_zstd_magic_is_plain_and_never_decoded() {
        let (_temp, archive) = init();
        // A whole-file capture of a `.zst` source starts with the zstd magic —
        // the explicit encoding (file name) decides, never content sniffing.
        let mut raw = vec![0x28, 0xB5, 0x2F, 0xFD];
        raw.extend_from_slice(b"not actually a zstd frame, but stored verbatim\n");
        let (sha, _, _, encoding) = archive
            .put_object_encoded(&raw, StorageHint::Raw)
            .unwrap();
        assert_eq!(encoding, ObjectEncoding::Plain);
        assert_eq!(archive.object_encoding(&sha), Some(ObjectEncoding::Plain));
        assert_eq!(archive.read_object_bytes(&sha).unwrap(), raw);
        // compact's worth test is size-based; this content does not compress,
        // so a compact run leaves the encoding alone.
        let report = crate::compact::compact(&archive, false).unwrap();
        assert_eq!(report.compressed, 0);
        assert_eq!(archive.object_encoding(&sha), Some(ObjectEncoding::Plain));
        assert_eq!(archive.read_object_bytes(&sha).unwrap(), raw);
    }

    #[test]
    fn dedupe_hits_either_encoding_and_never_duplicates() {
        let (_temp, archive) = init();
        let mut raw = Vec::new();
        for _ in 0..500 {
            raw.extend_from_slice(b"dedupe me\n");
        }
        let (sha, _, is_new, _) = archive
            .put_object_encoded(&raw, StorageHint::Compressible)
            .unwrap();
        assert!(is_new);
        // Same bytes with the opposite hint dedupe against the zstd object.
        let (sha2, _, is_new2, encoding2) = archive
            .put_object_encoded(&raw, StorageHint::Raw)
            .unwrap();
        assert_eq!(sha2, sha);
        assert!(!is_new2);
        assert_eq!(encoding2, ObjectEncoding::Zstd);
        assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);
    }

    #[test]
    fn reconcile_reports_storage_stats_and_dual_encoding() {
        let (_temp, archive) = init();
        let mut compressible = Vec::new();
        for _ in 0..500 {
            compressible.extend_from_slice(b"{\"k\":\"v\"}\n");
        }
        let (csha, clen, _, _) = archive
            .put_object_encoded(&compressible, StorageHint::Compressible)
            .unwrap();
        let noise = vec![7u8; 4096];
        let (nsha, nlen, _, _) = archive
            .put_object_encoded(&noise, StorageHint::Raw)
            .unwrap();
        let report = archive.reconcile(&[]).unwrap();
        assert_eq!(report.storage.zstd_objects, 1);
        assert_eq!(report.storage.zstd_original_bytes, clen);
        assert!(report.storage.zstd_stored_bytes < clen);
        assert_eq!(report.storage.plain_objects, 1);
        assert_eq!(report.storage.plain_original_bytes, nlen);
        assert_eq!(report.storage.plain_stored_bytes, nlen);
        assert_eq!(report.objects_on_disk, 2);
        assert!(report.dual_encoding.is_empty());
        let mut expected_orphans = vec![csha.clone(), nsha.clone()];
        expected_orphans.sort();
        assert_eq!(report.orphan_objects, expected_orphans);

        // Crash-window simulation: both encodings of one sha exist. The sha is
        // counted once on disk, flagged as dual, and reads still succeed.
        std::fs::write(archive.object_path(&csha), &compressible).unwrap();
        let report = archive.reconcile(&[]).unwrap();
        assert_eq!(report.objects_on_disk, 2);
        assert_eq!(report.dual_encoding, vec![csha.clone()]);
        assert_eq!(archive.object_encoding(&csha), Some(ObjectEncoding::Zstd));
        assert_eq!(archive.read_object_bytes(&csha).unwrap(), compressible);
    }

    #[test]
    fn streamed_compressed_object_round_trips_and_restarts_when_not_worth_it() {
        struct Dribble<'a> {
            data: &'a [u8],
            pos: usize,
            step: usize,
        }
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.pos >= self.data.len() {
                    return Ok(0);
                }
                let take = self.step.min(buf.len()).min(self.data.len() - self.pos);
                buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
                self.pos += take;
                Ok(take)
            }
        }
        impl RestartableRead for Dribble<'_> {
            fn reset(&mut self) -> io::Result<()> {
                self.pos = 0;
                Ok(())
            }
        }

        let (_temp, archive) = init();
        // Compressible stream: stored zstd, readback byte-exact.
        let mut raw = Vec::new();
        for _ in 0..8000 {
            raw.extend_from_slice(b"streaming line\n");
        }
        let mut reader = Dribble {
            data: &raw,
            pos: 0,
            step: 1024,
        };
        let (sha, len, _, encoding) = archive
            .put_object_stream_encoded(&mut reader, raw.len() as u64, StorageHint::Compressible)
            .unwrap();
        assert_eq!(encoding, ObjectEncoding::Zstd);
        assert_eq!(len, raw.len() as u64);
        assert_eq!(archive.read_object_bytes(&sha).unwrap(), raw);

        // Incompressible stream: the reader is rewound and the plain bytes are
        // stored in a second pass.
        let mut value: u64 = 0x2545_F491_4F6C_DD1D;
        let mut noise = Vec::with_capacity(64 * 1024);
        for _ in 0..64 * 1024 / 8 {
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            noise.extend_from_slice(&value.to_le_bytes());
        }
        let mut reader = Dribble {
            data: &noise,
            pos: 0,
            step: 999,
        };
        let (sha2, len2, _, encoding2) = archive
            .put_object_stream_encoded(&mut reader, noise.len() as u64, StorageHint::Compressible)
            .unwrap();
        assert_eq!(encoding2, ObjectEncoding::Plain);
        assert_eq!(len2, noise.len() as u64);
        assert_eq!(archive.read_object_bytes(&sha2).unwrap(), noise);
        assert_eq!(archive.reconcile(&[]).unwrap().staging_residue.len(), 0);
    }
}

#[cfg(test)]
mod journal_concurrency_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc as StdArc;
    use std::thread;
    use std::time::Duration;

    /// Serialises the probe tests: they share one process-wide hook, so two of
    /// them must never be armed at once.
    fn probe_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    /// Installs no hook itself; only guarantees the hook is cleared when the
    /// test ends (including a panic) and that no other probe test is running.
    struct ProbeGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ProbeGuard {
        fn new() -> ProbeGuard {
            let guard = probe_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ledger_probe::clear();
            ProbeGuard { _guard: guard }
        }
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            ledger_probe::clear();
        }
    }

    fn root_of(dir: &tempfile::TempDir) -> PathBuf {
        let root = dir.path().join("root");
        Archive::init(&root).expect("init runtime root");
        root
    }

    /// One synthetic journal line: sha = index, object = 0x1000 + index.
    fn staged(index: u64) -> (String, RecordRef) {
        (
            format!("{:064x}", index),
            RecordRef {
                object: format!("{:064x}", 0x1000 + index),
                offset: index * 3,
                len: 3,
                capture_id: format!("cap-{index}"),
            },
        )
    }

    /// Stage one synthetic record and append it.
    fn append_one(archive: &Archive, index: u64) -> Result<(), ArchiveError> {
        let (sha, reference) = staged(index);
        if archive.stage_record_ref(&sha, reference)? {
            archive.flush_records()?;
        }
        Ok(())
    }

    fn journal_lines(root: &Path) -> usize {
        match fs::read_to_string(root.join("state").join(records::RECORDS_TAIL_FILENAME)) {
            Ok(text) => text.lines().count(),
            Err(_) => 0,
        }
    }

    // -----------------------------------------------------------------------
    // P1: an append must not be able to land inside the compaction's
    // "stamp checked, about to remove" window.
    // -----------------------------------------------------------------------

    #[test]
    fn an_append_cannot_land_inside_a_compactions_removal_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        {
            let archive = Archive::open(&root).expect("open");
            append_one(&archive, 1).expect("first append writes the canonical");
            append_one(&archive, 2).expect("second append goes to the journal");
        }
        assert_eq!(journal_lines(&root), 1, "one journal line before the race");

        let _probe = ProbeGuard::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let fired = StdArc::new(AtomicBool::new(false));
        let release_rx = Mutex::new(release_rx);
        let own_root = root.clone();
        ledger_probe::set(StdArc::new(move |point, hook_root| {
            if point != ledger_probe::Point::CompactionTrimWindow
                || hook_root != own_root
                || fired.swap(true, Ordering::SeqCst)
            {
                return;
            }
            entered_tx.send(()).ok();
            // Released by the test once the append proved that it waits.
            let _ = release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(Duration::from_secs(30));
        }));

        let compaction_root = root.clone();
        let compaction = thread::spawn(move || {
            Archive::open(&compaction_root)
                .expect("compaction archive")
                .compact_records(false)
        });
        entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the compaction reached its removal window");

        // The append now has to wait: the journal is held by the window in which
        // an unlocked append would be deleted before it was ever merged.
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let append_root = root.clone();
        let appender = thread::spawn(move || {
            started_tx.send(()).ok();
            let archive = Archive::open(&append_root).expect("appender archive");
            let result = append_one(&archive, 3);
            done_tx.send(result.is_ok()).ok();
            result
        });
        started_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the appender started");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "an append must not run inside the compaction's removal window"
        );
        assert_eq!(
            journal_lines(&root),
            1,
            "nothing was appended while the window was open"
        );

        release_tx.send(()).ok();
        let report = compaction
            .join()
            .expect("compaction thread")
            .expect("compaction runs");
        assert!(report.performed, "{report:?}");
        assert!(
            report.tail_removed,
            "the journal was the one the compaction merged: {report:?}"
        );
        appender
            .join()
            .expect("appender thread")
            .expect("the append succeeds");

        // Both records survive with their own mappings: the compaction removed
        // the journal it merged, the append created a new one.
        let ledger = Archive::open(&root)
            .expect("open")
            .load_records()
            .expect("merged ledger");
        assert_eq!(
            ledger.records.len(),
            3,
            "{:?}",
            ledger.records.keys().collect::<Vec<_>>()
        );
        for index in 1..=3u64 {
            assert_mapping(&ledger, index);
        }
    }

    // -----------------------------------------------------------------------
    // P2: a read that spans a compaction must not mix the two states.
    // -----------------------------------------------------------------------

    #[test]
    fn a_read_that_spans_a_compaction_retries_instead_of_mixing_two_states() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        {
            let archive = Archive::open(&root).expect("open");
            append_one(&archive, 1).expect("canonical record");
            append_one(&archive, 2).expect("journal-only record");
        }
        let canonical_path = root.join("state").join(records::RECORDS_CANONICAL_FILENAME);
        let tail_path = root.join("state").join(records::RECORDS_TAIL_FILENAME);
        let canonical_before: serde_json::Value =
            serde_json::from_slice(&fs::read(&canonical_path).expect("canonical"))
                .expect("canonical parses");
        assert_eq!(
            canonical_before["records"]
                .as_object()
                .expect("records map")
                .len(),
            1
        );
        assert_eq!(journal_lines(&root), 1);

        let _probe = ProbeGuard::new();
        let fired = StdArc::new(AtomicBool::new(false));
        let hook_canonical = canonical_path.clone();
        let hook_journal = tail_path.clone();
        let own_root = root.clone();
        ledger_probe::set(StdArc::new(move |point, hook_root| {
            if point != ledger_probe::Point::ReadBetweenFiles
                || hook_root != own_root
                || fired.swap(true, Ordering::SeqCst)
            {
                return;
            }
            // Another process finishes a compaction right here: the canonical
            // gains the journal's entry and the journal goes away. A single-pass
            // reader would return the canonical it already read plus a journal
            // that no longer exists — a ledger state that never existed.
            let mut merged = canonical_before.clone();
            let line = fs::read_to_string(&hook_journal).expect("journal");
            let value: serde_json::Value =
                serde_json::from_str(line.trim()).expect("journal line parses");
            let sha = value["sha"].as_str().expect("sha").to_string();
            let mut entry = value.clone();
            entry.as_object_mut().expect("entry object").remove("sha");
            merged["records"][sha] = entry;
            fs::write(
                &hook_canonical,
                serde_json::to_vec_pretty(&merged).expect("canonical bytes"),
            )
            .expect("write canonical");
            fs::remove_file(&hook_journal).expect("remove journal");
        }));

        crate::cost::begin_recording();
        let ledger = Archive::open(&root)
            .expect("open")
            .load_records()
            .expect("merged ledger");
        let cost = crate::cost::finish_recording().expect("cost recording");

        let retries = cost
            .counters
            .get("records_read.retries")
            .copied()
            .unwrap_or(0);
        assert!(
            retries >= 1,
            "the read observed the moved stamp and retried: {cost:?}"
        );
        assert_eq!(
            ledger.records.len(),
            2,
            "a read never returns the half-applied state (canonical before the \
             rewrite plus a journal that is already gone)"
        );
        for index in 1..=2u64 {
            let (sha, expected) = staged(index);
            let actual = ledger.records.get(&sha).expect("record survives");
            assert_eq!(
                (
                    actual.object.as_str(),
                    actual.offset,
                    actual.len,
                    actual.capture_id.as_str()
                ),
                (
                    expected.object.as_str(),
                    expected.offset,
                    expected.len,
                    expected.capture_id.as_str()
                ),
                "record {index}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // "The journal is still the one I merged" is a content claim.
    // -----------------------------------------------------------------------

    #[test]
    fn a_journal_rewritten_to_the_same_length_is_not_the_one_that_was_merged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let archive = Archive::open(&root).expect("open");
        append_one(&archive, 1).expect("canonical record");
        append_one(&archive, 2).expect("journal record");
        let tail = root.join("state").join(records::RECORDS_TAIL_FILENAME);
        let (ledger, _stamp, read) = read_ledger(&root).expect("merged read");
        assert_eq!(ledger.records.len(), 2);
        assert!(archive
            .journal_is_the_one_merged(&tail, &read)
            .expect("the journal just read is the merged one"));

        let original = fs::read(&tail).expect("journal");
        let mut rewritten = original.clone();
        // Same length, different content: neither the size nor a size + mtime
        // stamp can tell these two journals apart, which is why the removal
        // compares the digest of the exact bytes that were merged.
        rewritten[0] = if original[0] == b'{' { b'[' } else { b'{' };
        fs::write(&tail, &rewritten).expect("rewrite the journal");
        assert_eq!(rewritten.len(), original.len());
        assert!(
            !archive
                .journal_is_the_one_merged(&tail, &read)
                .expect("check"),
            "a different journal of the same length must not be removed"
        );

        // The same bytes again: it is the merged journal even though its stamp
        // moved, so the canonical write plus removal still drops it.
        fs::write(&tail, &original).expect("restore the journal");
        assert!(archive
            .journal_is_the_one_merged(&tail, &read)
            .expect("check"));
    }

    // -----------------------------------------------------------------------
    // The compaction must keep a journal it cannot exclusively lock.
    // -----------------------------------------------------------------------
    #[test]
    fn a_compaction_that_cannot_lock_the_journal_keeps_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        {
            let archive = Archive::open(&root).expect("open");
            append_one(&archive, 1).expect("canonical record");
            append_one(&archive, 2).expect("journal-only record");
        }
        let holder = Archive::open(&root).expect("open");
        let _held = holder
            .acquire_lock_wait(RECORDS_TAIL_LOCK, 0)
            .expect("the journal lock is free");

        let report = Archive::open(&root)
            .expect("open")
            .compact_records(false)
            .expect("the compaction runs");
        assert!(report.performed, "{report:?}");
        assert!(
            !report.tail_removed,
            "a journal the compaction cannot lock is kept: {report:?}"
        );
        assert!(report.tail_kept_because_locked, "{report:?}");
        assert!(
            !report.tail_kept_because_changed,
            "the journal did not change, it was locked: {report:?}"
        );
        assert_eq!(journal_lines(&root), 1, "the journal is still there");

        // Everything is still readable, and the journal is only duplicates now.
        let ledger = Archive::open(&root)
            .expect("open")
            .load_records()
            .expect("merged ledger");
        assert_eq!(ledger.records.len(), 2);
        drop(_held);
        let retried = Archive::open(&root)
            .expect("open")
            .compact_records(false)
            .expect("the retry compacts");
        assert!(retried.tail_removed, "{retried:?}");
        assert_eq!(journal_lines(&root), 0);
    }

    // -----------------------------------------------------------------------
    // O-1: a lock that is being deleted is contention, not a broken disk.
    // -----------------------------------------------------------------------

    #[test]
    fn a_lock_that_is_being_deleted_counts_as_contention() {
        let path = PathBuf::from("locks").join(RECORDS_TAIL_LOCK);
        let denied = ArchiveError::Io(AppError::Io(
            path.clone(),
            io::Error::from(io::ErrorKind::PermissionDenied),
        ));
        assert!(
            lock_contended(&denied),
            "ERROR_ACCESS_DENIED on `create_new` is contention"
        );
        assert!(lock_contended(&ArchiveError::LockHeld(path.clone())));
        assert!(lock_contended(&ArchiveError::Io(AppError::Io(
            path.clone(),
            io::Error::from(io::ErrorKind::AlreadyExists)
        ))));
        // A genuine I/O failure is not contention and still fails the call.
        for permanent in [io::ErrorKind::NotFound, io::ErrorKind::InvalidInput] {
            assert!(
                !lock_contended(&ArchiveError::Io(AppError::Io(
                    path.clone(),
                    io::Error::from(permanent)
                ))),
                "{permanent:?} must stay a hard failure"
            );
        }
    }

    /// A path the journal lock cannot be created on: `create_new` then fails with
    /// `ERROR_ACCESS_DENIED` on Windows — the same class another thread's
    /// in-progress deletion produces, but deterministic.
    #[cfg(windows)]
    fn deny_journal_lock(root: &Path) -> PathBuf {
        let lock = root.join("locks").join(RECORDS_TAIL_LOCK);
        fs::create_dir(&lock).expect("a path the lock cannot be created on");
        lock
    }

    /// O-1: the compaction's 50 ms budget used to end in a hard `Io` error when
    /// the journal lock kept reporting "access denied". The canonical is already
    /// written at that point, so the journal is only duplicates: keep it and
    /// trim it next round, like a held lock.
    #[cfg(windows)]
    #[test]
    fn a_denied_journal_lock_keeps_the_journal_instead_of_failing_the_compaction() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        {
            let archive = Archive::open(&root).expect("open");
            append_one(&archive, 1).expect("canonical record");
            append_one(&archive, 2).expect("journal-only record");
        }
        let lock = deny_journal_lock(&root);

        let report = Archive::open(&root)
            .expect("open")
            .compact_records(false)
            .expect("a journal that cannot be locked is kept, not a hard failure");
        assert!(report.performed, "{report:?}");
        assert!(!report.tail_removed, "{report:?}");
        assert!(
            report.tail_kept_because_locked,
            "an access-denied journal lock counts as held: {report:?}"
        );
        assert!(!report.tail_kept_because_changed, "{report:?}");
        assert_eq!(journal_lines(&root), 1, "the journal is still there");
        assert_eq!(
            Archive::open(&root)
                .expect("open")
                .load_records()
                .expect("merged ledger")
                .records
                .len(),
            2,
            "nothing was lost"
        );

        fs::remove_dir(&lock).expect("release the lock path");
        let retried = Archive::open(&root)
            .expect("open")
            .compact_records(false)
            .expect("the retry compacts");
        assert!(retried.tail_removed, "{retried:?}");
        assert_eq!(journal_lines(&root), 0);
    }

    /// O-1 for the other caller: a whole-ledger replacement cannot "keep the
    /// journal, trim next round" (the surviving lines would resurrect the entries
    /// the call was asked to drop), so it refuses — with the class the caller can
    /// act on (`LockHeld`), not a raw `Io`, and without writing anything.
    #[cfg(windows)]
    #[test]
    fn a_denied_journal_lock_refuses_the_replacement_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        {
            let archive = Archive::open(&root).expect("open");
            append_one(&archive, 1).expect("canonical record");
            append_one(&archive, 2).expect("journal-only record");
        }
        let canonical = root.join("state").join(records::RECORDS_CANONICAL_FILENAME);
        let canonical_before = fs::read(&canonical).expect("canonical bytes");
        let lock = deny_journal_lock(&root);

        let archive = Archive::open(&root).expect("open");
        let mut replacement = RecordsFile {
            schema: RECORDS_SCHEMA.to_string(),
            records: BTreeMap::new(),
        };
        replacement.records.insert(staged(9).0, staged(9).1);
        let error = archive
            .save_records(&replacement)
            .expect_err("a replacement that cannot lock the journal refuses");
        assert!(
            matches!(error, ArchiveError::LockHeld(_)),
            "transient access-denied is reported as a held lock: {error:?}"
        );
        assert_eq!(
            fs::read(&canonical).expect("canonical bytes"),
            canonical_before,
            "nothing was written before the lock was taken"
        );
        assert_eq!(journal_lines(&root), 1, "the journal is untouched");

        fs::remove_dir(&lock).expect("release the lock path");
        Archive::open(&root)
            .expect("open")
            .save_records(&replacement)
            .expect("the retry replaces the ledger");
        assert_eq!(journal_lines(&root), 0, "the journal is gone after the retry");
    }

    // -----------------------------------------------------------------------
    // The degraded path: no journal lock, so no journal write.
    // -----------------------------------------------------------------------

    /// Leave `locks/records-tail.lock` behind exactly as a hard kill does.
    fn hold_journal_lock(root: &Path) {
        fs::write(
            root.join("locks").join(RECORDS_TAIL_LOCK),
            b"pid=0 acquired_at_unix_ms=0\n",
        )
        .expect("hold the journal lock");
    }

    fn release_journal_lock(root: &Path) {
        fs::remove_file(root.join("locks").join(RECORDS_TAIL_LOCK))
            .expect("release the journal lock");
    }

    /// The entries of the canonical file alone (what an old reader sees).
    fn canonical_records(root: &Path) -> BTreeMap<String, RecordRef> {
        let path = root.join("state").join(records::RECORDS_CANONICAL_FILENAME);
        match fs::read(&path) {
            Ok(raw) => {
                serde_json::from_slice::<RecordsFile>(&raw)
                    .expect("the canonical parses")
                    .records
            }
            Err(_) => BTreeMap::new(),
        }
    }

    fn assert_mapping(ledger: &RecordsFile, index: u64) {
        let (sha, expected) = staged(index);
        let actual = ledger
            .records
            .get(&sha)
            .unwrap_or_else(|| panic!("record {index} is in the ledger"));
        assert_eq!(
            (
                actual.object.as_str(),
                actual.offset,
                actual.len,
                actual.capture_id.as_str()
            ),
            (
                expected.object.as_str(),
                expected.offset,
                expected.len,
                expected.capture_id.as_str()
            ),
            "record {index} keeps its mapping"
        );
    }

    #[test]
    fn a_flush_without_the_journal_lock_promotes_the_entries_and_never_writes_the_journal() {
        // A journal lock left behind by a hard kill must neither block capture
        // forever nor let a flush write the journal unlocked (a line written
        // outside the lock can be deleted inside a compaction's removal window).
        // The entries go into the canonical instead, under the compaction lock.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        {
            let archive = Archive::open(&root).expect("open");
            append_one(&archive, 1).expect("canonical record");
            append_one(&archive, 2).expect("journal record");
        }
        assert_eq!(
            journal_lines(&root),
            1,
            "one journal line before the timeout"
        );
        hold_journal_lock(&root);

        crate::cost::begin_recording();
        let archive = Archive::open(&root).expect("open");
        append_one(&archive, 3).expect("the flush promotes instead of writing the journal");
        let stats = archive.ledger_stats();
        let cost = crate::cost::finish_recording().expect("cost recording");

        assert_eq!(
            journal_lines(&root),
            1,
            "no journal byte may be written without the journal lock"
        );
        assert_eq!(stats.records_promotions, 1, "{stats:?}");
        assert_eq!(
            stats.records_flushes, 0,
            "a promotion is not an append: {stats:?}"
        );
        assert_eq!(cost.counters.get("records_write.promoted"), Some(&1));
        assert!(
            canonical_records(&root).contains_key(&staged(3).0),
            "the entry is in the canonical, not only in memory"
        );

        let ledger = Archive::open(&root)
            .expect("open")
            .load_records()
            .expect("merged ledger");
        assert_eq!(ledger.records.len(), 3);
        for index in 1..=3u64 {
            assert_mapping(&ledger, index);
        }

        // The stale file is the only thing that keeps the degraded path: once it
        // is gone the next flush appends to the journal again.
        release_journal_lock(&root);
        append_one(&archive, 4).expect("the normal append path is back");
        assert_eq!(journal_lines(&root), 2, "the journal is used again");
        let ledger = Archive::open(&root)
            .expect("open")
            .load_records()
            .expect("merged ledger");
        assert_eq!(ledger.records.len(), 4);
    }

    #[test]
    fn an_entry_flushed_while_a_compaction_removes_the_journal_is_never_lost() {
        // The interleaving the unlocked append used to lose: the compaction has
        // already compared the journal with what it merged and is inside its
        // removal window; a flush that cannot take the journal lock must not put
        // a line into that journal. It keeps the entry staged and fails the
        // round (nothing committed), and the retry after the window commits it.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        {
            let archive = Archive::open(&root).expect("open");
            append_one(&archive, 1).expect("canonical record");
            append_one(&archive, 2).expect("journal record");
        }
        assert_eq!(journal_lines(&root), 1);

        let _probe = ProbeGuard::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let fired = StdArc::new(AtomicBool::new(false));
        let release_rx = Mutex::new(release_rx);
        let own_root = root.clone();
        ledger_probe::set(StdArc::new(move |point, hook_root| {
            if point != ledger_probe::Point::CompactionTrimWindow
                || hook_root != own_root
                || fired.swap(true, Ordering::SeqCst)
            {
                return;
            }
            entered_tx.send(()).ok();
            let _ = release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(Duration::from_secs(30));
        }));

        let compaction_root = root.clone();
        let compaction = thread::spawn(move || {
            Archive::open(&compaction_root)
                .expect("compaction archive")
                .compact_records(false)
        });
        entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the compaction reached its removal window");

        // The journal lock is held by that window, and the promotion cannot run
        // either: the window is inside the compaction, which holds the
        // compaction lock for its whole run. So the flush fails with its entries
        // still staged and nothing on disk.
        let archive = StdArc::new(Archive::open(&root).expect("flusher archive"));
        let flusher_archive = StdArc::clone(&archive);
        let (flushed_tx, flushed_rx) = mpsc::channel();
        let flusher = thread::spawn(move || {
            let (sha, reference) = staged(3);
            flusher_archive
                .stage_record_ref(&sha, reference)
                .expect("stage the entry");
            // Bounded: one journal-lock budget plus one compaction-lock budget.
            flushed_tx
                .send(flusher_archive.flush_records().is_ok())
                .ok();
        });
        let committed = flushed_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the degraded flush returns within its budget");
        assert!(
            !committed,
            "a flush inside the removal window cannot commit an entry"
        );
        assert_eq!(
            journal_lines(&root),
            1,
            "nothing may be written to the journal without its lock"
        );
        assert!(
            !canonical_records(&root).contains_key(&staged(3).0),
            "and nothing may be promoted while the compaction lock is held"
        );

        flusher.join().expect("flusher thread");
        release_tx.send(()).ok();
        let report = compaction
            .join()
            .expect("compaction thread")
            .expect("compaction runs");
        assert!(report.performed, "{report:?}");
        assert!(
            report.tail_removed,
            "the journal was the one the compaction merged: {report:?}"
        );
        assert_eq!(journal_lines(&root), 0);

        // The staged entry survived the failure in memory: the retry commits it
        // (a whole capture round would re-capture and re-stage it instead; see
        // `a_failed_ledger_flush_leaves_the_source_watermark_unadvanced`).
        archive.flush_records().expect("the retry appends");
        let ledger = Archive::open(&root)
            .expect("open")
            .load_records()
            .expect("merged ledger");
        assert_eq!(ledger.records.len(), 3);
        for index in 1..=3u64 {
            assert_mapping(&ledger, index);
        }
    }

    #[test]
    fn an_automatic_compaction_holds_the_compaction_lock_for_its_whole_run() {
        // Two automatic compactions used to be able to interleave: the guard was
        // taken in a `match` arm and dropped at the end of that arm, before the
        // rewrite. One of them could then overwrite the other's newer canonical.
        // The lock has to cover the whole run, which is what makes the canonical
        // write of the degraded path exclusive with every journal removal.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let _probe = ProbeGuard::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let fired = StdArc::new(AtomicBool::new(false));
        let nested = StdArc::new(Mutex::new(None));
        let hook_root = root.clone();
        let own_root = root.clone();
        let nested_result = StdArc::clone(&nested);
        ledger_probe::set(StdArc::new(move |point, probe_root| {
            if point != ledger_probe::Point::CompactionTrimWindow
                || probe_root != own_root
                || fired.swap(true, Ordering::SeqCst)
            {
                return;
            }
            // A second compaction starts while the first is still inside its
            // run (its removal window): it must be refused, not interleaved.
            // The result is stored *before* the test is signalled, so the
            // assertion cannot read the slot while this hook is still running.
            let attempt = Archive::open(&hook_root)
                .and_then(|archive| archive.compact_records(false))
                .map(|report| report.performed);
            *nested_result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(attempt);
            entered_tx.send(()).ok();
        }));

        // A fresh root compacts automatically at the end of its first flush.
        let archive = Archive::open(&root).expect("open");
        append_one(&archive, 1).expect("the first flush appends and compacts");
        entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the automatic compaction reached its removal window");
        let attempt = nested
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        match attempt {
            Some(Err(ArchiveError::LockHeld(_))) => {}
            other => {
                panic!("a second compaction must report the held compaction lock, got {other:?}")
            }
        }
    }

    #[test]
    fn a_failed_ledger_flush_leaves_the_source_watermark_unadvanced() {
        // The round's write order is records first, sources second: a ledger
        // flush that cannot commit fails `CaptureSession::finish` before the
        // source state is written. The watermark therefore stays where it was,
        // so the next round re-reads the same bytes and re-stages the entries —
        // which is what makes "keep the staged entries and retry" safe.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let archive = Archive::open(&root).expect("open");
        hold_journal_lock(&root);
        let _compaction = archive
            .acquire_named_lock(RECORDS_COMPACT_LOCK)
            .expect("hold the compaction lock");

        let session = archive.begin_capture_session();
        let (sha, reference) = staged(1);
        archive
            .stage_record_ref(&sha, reference)
            .expect("stage a record");
        let mut state = SourcesFile {
            schema: STATE_SCHEMA.to_string(),
            sources: BTreeMap::new(),
        };
        state.sources.insert(
            "session-1.jsonl".to_string(),
            SourceState {
                provider: "claude_code".to_string(),
                status: "captured".to_string(),
                watermark: Some(Watermark {
                    len: 4096,
                    sha256: "0".repeat(64),
                    fingerprint: SourceFingerprint {
                        mtime_ms: 1_700_000_000_000,
                        len: 4096,
                        head_sha256: "1".repeat(64),
                        tail_sha256: "2".repeat(64),
                    },
                    capture_id: "cap-1".to_string(),
                    captured_at_ms: 1_700_000_000_000,
                    offset: 4096,
                    line: 3,
                    records: 3,
                    generation: 1,
                    head_probe: String::new(),
                    anchor_probe: String::new(),
                    probe_len: 0,
                }),
                ..Default::default()
            },
        );
        archive
            .save_sources(&state)
            .expect("stage the source state");

        let outcome = session.finish();
        assert!(
            outcome.is_err(),
            "a round whose ledger write failed must not report success: {outcome:?}"
        );
        // Nothing was committed: no ledger entry, and the watermark is still
        // where it was, so the next round re-captures the same bytes.
        assert!(
            !root
                .join("state")
                .join(records::RECORDS_CANONICAL_FILENAME)
                .exists(),
            "no canonical entry"
        );
        assert!(
            !root.join("state").join("sources.json").exists(),
            "no advanced watermark"
        );

        // With the locks free, the same staged state commits: the retry is what
        // the next round does after re-reading the source.
        drop(_compaction);
        release_journal_lock(&root);
        archive
            .flush_records()
            .expect("the retry writes the ledger");
        archive
            .flush_sources()
            .expect("the retry writes the watermark");
        let ledger = Archive::open(&root)
            .expect("open")
            .load_records()
            .expect("merged ledger");
        assert_mapping(&ledger, 1);
        let sources = Archive::open(&root)
            .expect("open")
            .load_sources()
            .expect("sources");
        assert_eq!(
            sources
                .sources
                .get("session-1.jsonl")
                .and_then(|entry| entry.watermark.as_ref().map(|watermark| watermark.len)),
            Some(4096)
        );
    }
}
