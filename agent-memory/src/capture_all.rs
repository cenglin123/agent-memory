//! Multi-source capture driver: `capture-all --config <recall.json>`.
//!
//! One invocation enumerates every enabled live source from the explicit
//! configuration (no default-root discovery, ever), captures each file through
//! the existing single-source engine (`capture::capture_source`, which owns the
//! runtime-root single-writer lock per file), and keeps its own *skip ledger*
//! at `<runtime-root>/state/capture-all.json` so unchanged JSONL files are not
//! re-read at all.
//!
//! Enumeration rules (the single definition, mirrored in `agent-memory/README.md`):
//! - `claude_code` (provider `claude`): every `*.jsonl` file recursively under
//!   the configured root (this includes `<project>/<session>.jsonl`,
//!   `<project>/<session>/subagents/*.jsonl` and any `tool-results/*.jsonl`).
//! - `codex` (provider `codex`): `rollout-*.jsonl` / `rollout-*.jsonl.zst`
//!   recursively under `<root>/sessions/` and, when the directory exists,
//!   `<root>/archived_sessions/`; when both the plain and the `.zst` form of a
//!   rollout exist, only the plain form is captured (Codex is actively writing
//!   it) — same rule as the recall provider.
//! - `opencode` (provider `opencode`): `<root>/opencode.db` (or the explicit
//!   `db` path from the configuration) as the SQLite evidence source, plus the
//!   legacy JSON storage files under `<storage>/session/`, `<storage>/message/`
//!   and `<storage>/part/` (`*.json` only; the fixed `json-legacy-v1` layout).
//! - `dsh` (provider `dsh`): every `<root>/<project-slug>/<session-dir>` with
//!   exactly one selected carrier — the highest generation file the reader
//!   understands (`session.jsonl.zstd`, `session.vN.jsonl.zstd`, or the plain
//!   `.jsonl` form). A session directory whose highest generation is unknown is
//!   skipped and counted in the per-source `dsh` report. Evidence is the
//!   decompressed logical JSONL stream, captured incrementally by logical line
//!   (see `capture_dsh`), never the compressed carrier.
//!
//! All file decisions are metadata-only (size/mtime); session bodies never
//! enter the driver report. The SQLite source is always handed to the capture
//! engine (its own watermark decides the cost); `--session-since` is filtered
//! by file mtime for file sources and reported as an unsupported option for
//! SQLite until S1 adds the pass-through parameter.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::archive::{now_ms, Archive, ArchiveError, LedgerStats, ReconcileReport};
use crate::capture::{canonical_key, capture_source_with_options, CaptureOptions, CaptureReport};
use crate::capture_dsh;
use crate::codex_rollout_file;
use crate::config::atomic_write;
use crate::cost::{self, Cost};
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::providers::dsh;

/// Ledger file lives beside the capture watermarks; it is driver bookkeeping
/// (deletable, rebuilt by a full rescan) and never evidence.
pub const LEDGER_VERSION: u32 = 1;
pub const LEDGER_FILENAME: &str = "capture-all.json";

const CODE_SOURCE_MISSING: &str = "source_missing";
const CODE_ENUM_IO: &str = "enum_io";
const CODE_STAT_IO: &str = "stat_io";

/// CLI exit codes for the driver (stable contract, same table as
/// `src/bin/agent-memory.rs`: 2 usage · 3 io · 4 lock held · 7 capture failed).
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_IO: u8 = 3;
pub const EXIT_LOCK: u8 = 4;
pub const EXIT_CAPTURE: u8 = 7;

// ---------------------------------------------------------------------------
// Options / errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CaptureAllOptions {
    /// Explicit runtime root; wins over the `archive` source root.
    pub root: Option<String>,
    /// Access domain recorded on every captured object/record.
    pub domain: String,
    /// Restrict the run to one configured source id.
    pub only_source: Option<String>,
    /// RFC3339 instant: skip file sources whose mtime is older.
    pub session_since: Option<String>,
    /// Enumerate only; never write (not even the ledger).
    pub plan: bool,
    /// Exit 0 with `skipped=locked` when another writer holds the capture lock.
    pub skip_if_locked: bool,
}

/// Hard failures that happen before any report exists. The exit codes follow
/// the existing CLI contract (2 usage, others via the archive error mapping).
pub type CaptureAllSetupError = (u8, String);

// ---------------------------------------------------------------------------
// Ledger (driver skip bookkeeping)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerFile {
    pub size: u64,
    pub mtime_ms: i64,
    #[serde(default)]
    pub capture_id: Option<String>,
    #[serde(default)]
    pub captured_at_ms: Option<i64>,
    /// DSH only: carrier relative path this entry describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh_rel_path: Option<String>,
    /// DSH only: committed logical lines at capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh_lines: Option<u64>,
    /// DSH only: digest of the last committed logical line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh_last_line_sha256: Option<String>,
    /// DSH only: the old→new carrier link when this run restarted (generation
    /// switch or a rewritten log). Old evidence is never deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh_previous: Option<DshLink>,
}

/// One recorded DSH discontinuity: which carrier replaced which, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DshLink {
    pub rel_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_rel_path: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerSource {
    pub provider: String,
    pub files: BTreeMap<String, LedgerFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureLedger {
    pub version: u32,
    #[serde(default)]
    pub updated_at_ms: i64,
    pub sources: BTreeMap<String, LedgerSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerStatus {
    /// Ledger read and applied; unchanged files are skipped.
    Ok,
    /// No ledger yet (first run).
    Missing,
    /// Unreadable or wrong version: everything is re-captured and the ledger
    /// is rebuilt. Never an error by itself.
    Corrupt,
}

impl LedgerStatus {
    fn as_str(&self) -> &'static str {
        match self {
            LedgerStatus::Ok => "ok",
            LedgerStatus::Missing => "missing",
            LedgerStatus::Corrupt => "corrupt",
        }
    }
}

fn ledger_path(root: &Path) -> PathBuf {
    root.join("state").join(LEDGER_FILENAME)
}

fn load_ledger(root: &Path) -> (CaptureLedger, LedgerStatus) {
    let path = ledger_path(root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (
                CaptureLedger {
                    version: LEDGER_VERSION,
                    updated_at_ms: 0,
                    sources: BTreeMap::new(),
                },
                LedgerStatus::Missing,
            );
        }
        // Any other read failure forces the safe path: a full rescan with a
        // rebuilt ledger (rescanning is always correct, skipping is not).
        Err(_) => {
            return (
                CaptureLedger {
                    version: LEDGER_VERSION,
                    updated_at_ms: 0,
                    sources: BTreeMap::new(),
                },
                LedgerStatus::Corrupt,
            );
        }
    };
    match serde_json::from_slice::<CaptureLedger>(&bytes) {
        Ok(ledger) if ledger.version == LEDGER_VERSION => (ledger, LedgerStatus::Ok),
        _ => (
            CaptureLedger {
                version: LEDGER_VERSION,
                updated_at_ms: 0,
                sources: BTreeMap::new(),
            },
            LedgerStatus::Corrupt,
        ),
    }
}

fn save_ledger(root: &Path, ledger: &CaptureLedger) -> Result<(), ArchiveError> {
    let mut text = serde_json::to_string_pretty(ledger)
        .map_err(|e| ArchiveError::Corrupt(format!("capture-all ledger: {e}")))?;
    text.push('\n');
    atomic_write(&ledger_path(root), text.as_bytes())
        .map_err(|e| ArchiveError::Io(e))
}

// ---------------------------------------------------------------------------
// Report (metadata only: paths, counts, error codes — never session bodies)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SourceError {
    pub path: String,
    pub code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceReport {
    pub source_id: String,
    pub kind: String,
    pub provider: String,
    pub root: String,
    pub files_total: u64,
    pub files_captured: u64,
    pub files_skipped_unchanged: u64,
    pub files_skipped_since: u64,
    pub files_failed: u64,
    pub records_added: u64,
    pub bytes_added: u64,
    pub bytes_total: u64,
    pub unsupported_option: Vec<String>,
    pub errors: Vec<SourceError>,
    /// Present for a `dsh` source only (generation selection, damaged carriers,
    /// discontinuity links). Metadata only; never a session body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh: Option<DshReport>,
    /// §A: how many files fell into each change classification, and how many
    /// bytes a forced recapture had to read again in full.
    pub changes: ChangeCounts,
    pub duration_ms: u64,
    /// §A measurement of this source's work (counters only, never content).
    pub cost: Cost,
}

/// Per-source file change classification of one capture run (§A).
///
/// One file per classification (a JSONL / DSH carrier, or the SQLite source
/// itself). `forced_recapture_bytes` counts the bytes read *because* a
/// discontinuity forced a whole-file read, which is the number the `same
/// length rewrite` discussion needs; an initial capture is counted separately
/// and is not a "re-read".
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChangeCounts {
    pub initial: u64,
    pub append: u64,
    pub same_length_rewrite: u64,
    pub prefix_rewritten: u64,
    pub length_regression: u64,
    pub legacy_watermark_without_probes: u64,
    /// Forced recaptures whose reason is none of the named ones.
    pub other_forced: u64,
    pub skipped_unchanged: u64,
    /// Files skipped by the `--session-since` mtime filter (no read at all).
    pub skipped_since: u64,
    pub degraded: u64,
    pub source_missing: u64,
    pub failed: u64,
    /// SQLite only: a new chunked baseline (not a forced file re-read).
    pub baseline: u64,
    pub delta: u64,
    pub recovered_after_crash: u64,
    /// Files whose whole content was re-read because of a forced recapture.
    pub forced_recapture_files: u64,
    /// Bytes read for those forced recaptures (whole file each).
    pub forced_recapture_bytes: u64,
}

impl ChangeCounts {
    /// Fold one source's capture report into the classification counts.
    fn observe(&mut self, report: &CaptureReport) {
        match report.change.as_str() {
            "initial" => self.initial += 1,
            "append" => self.append += 1,
            "skipped_unchanged" => self.skipped_unchanged += 1,
            "degraded" => self.degraded += 1,
            "source_missing" => self.source_missing += 1,
            "failed" => self.failed += 1,
            "baseline" => self.baseline += 1,
            "delta" => self.delta += 1,
            "recovered_after_crash" => self.recovered_after_crash += 1,
            "forced_recapture" => {
                self.forced_recapture_files += 1;
                self.forced_recapture_bytes = self
                    .forced_recapture_bytes
                    .saturating_add(report.file_len);
                match report.change_reason.as_deref() {
                    Some("same_length_rewrite") => self.same_length_rewrite += 1,
                    Some("prefix_rewritten") => self.prefix_rewritten += 1,
                    Some("length_regression") => self.length_regression += 1,
                    Some("legacy_watermark_without_probes") => {
                        self.legacy_watermark_without_probes += 1
                    }
                    _ => self.other_forced += 1,
                }
            }
            _ => {}
        }
    }
}

/// DSH-specific per-source facts (metadata only).
#[derive(Debug, Clone, Default, Serialize)]
pub struct DshReport {
    /// Session directories discovered under the configured root.
    pub sessions_total: u64,
    /// Session directories whose highest generation this build does not read;
    /// they are skipped, never guessed.
    pub sessions_unsupported: u64,
    /// Their `<project>/<session-dir>` relative paths.
    pub unsupported: Vec<String>,
    /// Carriers whose trailing frame was torn or failed its checksum.
    pub partial_tail: u64,
    /// Carriers with an unreadable interior frame (lines after it are lost).
    pub corrupt: u64,
    /// Carriers that hit the logical read ceiling; only the prefix was archived.
    pub truncated: u64,
    /// Newly committed lines that are not valid JSON.
    pub malformed_lines: u64,
    /// Carrier links recorded by this run, by reason.
    pub links: Vec<DshLink>,
}

#[derive(Debug, Serialize)]
pub struct TotalsReport {
    pub files_total: u64,
    pub files_captured: u64,
    pub files_skipped_unchanged: u64,
    pub files_skipped_since: u64,
    pub files_failed: u64,
    pub records_added: u64,
    pub bytes_added: u64,
    pub errors: u64,
}

#[derive(Debug, Serialize)]
pub struct CaptureAllReport {
    pub ok: bool,
    pub plan: bool,
    pub root: String,
    /// `locked` when `--skip-if-locked` observed a held lock; otherwise null.
    pub skipped: Option<String>,
    /// Ledger state for this run: `ok | missing | corrupt`.
    pub ledger: String,
    pub session_since: Option<String>,
    pub sources: Vec<SourceReport>,
    pub totals: TotalsReport,
    pub duration_ms: u64,
    /// §A measurement of the whole run (counters only, never content).
    pub cost: Cost,
    /// The one round-level reconcile (object directory vs committed
    /// manifests), or `null` when it could not run — in which case
    /// `reconcile_error` names why and `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconcile: Option<ReconcileReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconcile_error: Option<String>,
    /// How often this round touched the record ledger and the source state.
    /// One round reads and writes each ledger at most once.
    pub ledgers: LedgerStats,
    /// Last automatic record-ledger compaction failure, if any. The journal is
    /// durable either way; this only discloses that the canonical rewrite did
    /// not happen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records_compact_error: Option<String>,
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

struct Enumerated {
    files: Vec<PathBuf>,
    errors: Vec<SourceError>,
    /// True when the SQLite database path is part of this source (never
    /// ledger-skipped; its watermark decides the cost).
    has_sqlite: bool,
    /// DSH only: the reader's inventory (highest generation per session dir).
    dsh: Option<dsh::DshInventory>,
}

fn push_enum_error(errors: &mut Vec<SourceError>, path: &Path, code: &str) {
    errors.push(SourceError {
        path: path.to_string_lossy().to_string(),
        code: code.to_string(),
    });
}

/// Recursive directory walk: deterministic order, symlinks never followed,
/// entries below the top level that cannot be listed are reported and skipped.
fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<SourceError>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            push_enum_error(errors, dir, CODE_ENUM_IO);
            return;
        }
    };
    let mut paths: Vec<(PathBuf, bool)> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => match entry.file_type() {
                Ok(kind) if kind.is_symlink() => continue,
                Ok(kind) => paths.push((entry.path(), kind.is_dir())),
                Err(_) => push_enum_error(errors, &entry.path(), CODE_ENUM_IO),
            },
            Err(_) => push_enum_error(errors, dir, CODE_ENUM_IO),
        }
    }
    paths.sort();
    for (path, is_dir) in paths {
        if is_dir {
            walk_dir(&path, out, errors);
        } else {
            out.push(path);
        }
    }
}

fn is_sqlite_db_path(path: &Path) -> bool {
    crate::capture_sqlite::is_sqlite_file(path) || crate::capture_sqlite::looks_like_sqlite_path(path)
}

/// The central enumeration rules (see the module docs). The configured roots
/// are used verbatim — nothing here discovers or derives a default root.
fn enumerate_source(source: &SourceConfig) -> Enumerated {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut has_sqlite = false;
    let mut dsh_inventory = None;
    let root = Path::new(&source.root);
    match source.kind {
        SourceKind::ClaudeCode => {
            if !root.is_dir() {
                push_enum_error(&mut errors, root, CODE_SOURCE_MISSING);
            } else {
                walk_dir(root, &mut files, &mut errors);
                files.retain(|path| {
                    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                });
            }
        }
        SourceKind::Codex => {
            let mut bases = Vec::new();
            let archived = root.join("archived_sessions");
            if root.join("sessions").is_dir() {
                bases.push(root.join("sessions"));
            }
            if archived.is_dir() {
                bases.push(archived);
            }
            if bases.is_empty() {
                push_enum_error(&mut errors, root.join("sessions").as_path(), CODE_SOURCE_MISSING);
            } else {
                for base in bases {
                    walk_dir(&base, &mut files, &mut errors);
                }
                files.retain(|path| {
                    codex_rollout_file::is_rollout_file(path)
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("rollout-"))
                });
                files = codex_rollout_file::dedupe_siblings(files);
            }
        }
        SourceKind::Opencode => {
            let db = match source.db.as_deref() {
                Some(db) => PathBuf::from(db),
                None => root.join("opencode.db"),
            };
            let storage = match source.storage.as_deref() {
                Some(storage) => PathBuf::from(storage),
                None => root.join("storage"),
            };
            if db.is_file() {
                has_sqlite = true;
                files.push(db.clone());
            }
            if storage.is_dir() {
                // Fixed legacy layout (`json-legacy-v1`): nothing else under
                // the storage root is scanned.
                let mut legacy = Vec::new();
                for subdir in ["session", "message", "part"] {
                    let dir = storage.join(subdir);
                    if dir.is_dir() {
                        walk_dir(&dir, &mut legacy, &mut errors);
                    }
                }
                legacy.retain(|path| {
                    path.extension().and_then(|e| e.to_str()) == Some("json")
                });
                files.extend(legacy);
            } else if !has_sqlite {
                push_enum_error(&mut errors, &db, CODE_SOURCE_MISSING);
            }
            files.sort();
            files.dedup();
        }
        // DSH: enumeration is the reader's own inventory (highest generation per
        // session directory; unknown generations are reported, not guessed).
        SourceKind::Dsh => {
            if !root.is_dir() {
                push_enum_error(&mut errors, root, CODE_SOURCE_MISSING);
            } else {
                match dsh::inventory(root) {
                    Ok(inventory) => {
                        files = inventory
                            .sessions
                            .iter()
                            .map(|session| session.path.clone())
                            .collect();
                        dsh_inventory = Some(inventory);
                    }
                    Err(_) => push_enum_error(&mut errors, root, CODE_ENUM_IO),
                }
            }
        }
        // KB / device / archive sources are not capture sources.
        _ => {}
    }
    files.sort();
    Enumerated {
        files,
        errors,
        has_sqlite,
        dsh: dsh_inventory,
    }
}

fn capture_provider(kind: SourceKind) -> Option<&'static str> {
    match kind {
        SourceKind::ClaudeCode => Some("claude"),
        SourceKind::Codex => Some("codex"),
        SourceKind::Opencode => Some("opencode"),
        SourceKind::Dsh => Some("dsh"),
        _ => None,
    }
}

fn error_code(error: &ArchiveError) -> &'static str {
    match error {
        ArchiveError::LockHeld(_) => "lock_held",
        ArchiveError::AccessDenied(_) => "access_denied",
        ArchiveError::NotFound(_) => "not_found",
        ArchiveError::Integrity(_) => "integrity",
        ArchiveError::Corrupt(_) => "corrupt",
        ArchiveError::LimitExceeded(_) => "limit_exceeded",
        ArchiveError::Embedding(_) => "embedding",
        ArchiveError::NotInitialized(_) => "not_initialized",
        ArchiveError::Io(_) => "io",
    }
}

/// File stamp used for both the `--session-since` filter and the skip ledger
/// (same millisecond rounding as the archive fingerprint).
fn file_stamp(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    Some((meta.len(), mtime_ms))
}

fn resolve_runtime_root(config: &RecallConfig, options: &CaptureAllOptions) -> Result<String, CaptureAllSetupError> {
    if let Some(root) = options.root.as_deref() {
        if root.trim().is_empty() {
            return Err((
                EXIT_USAGE,
                "--root must not be empty".to_string(),
            ));
        }
        return Ok(root.to_string());
    }
    let archive = config.sources.iter().find(|s| s.kind == SourceKind::Archive);
    match archive {
        Some(source) => Ok(source.root.clone()),
        None => Err((
            EXIT_USAGE,
            "recall.json has no archive source and --root was not given; \
             add an archive source (64 -ArchiveRoot) or pass --root"
                .to_string(),
        )),
    }
}

fn select_sources<'a>(
    config: &'a RecallConfig,
    only_source: Option<&str>,
) -> Result<Vec<&'a SourceConfig>, CaptureAllSetupError> {
    let mut selected: Vec<&SourceConfig> = config
        .sources
        .iter()
        .filter(|source| {
            source.enabled && capture_provider(source.kind).is_some()
        })
        .collect();
    if let Some(id) = only_source {
        let source = config.source(id).ok_or_else(|| {
            (
                EXIT_USAGE,
                format!("--source {id:?} is not a configured source id"),
            )
        })?;
        if !source.enabled {
            return Err((
                EXIT_USAGE,
                format!("--source {id:?} is configured but not enabled"),
            ));
        }
        if capture_provider(source.kind).is_none() {
            return Err((
                EXIT_USAGE,
                format!("--source {id:?} has kind {:?}, which has no capture files", source.kind.as_str()),
            ));
        }
        selected.retain(|source| source.source_id == id);
    }
    Ok(selected)
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Run the driver. Returns the report plus the process exit code
/// (`0` ok, `7` any source error), or a setup error.
pub fn capture_all(
    config: &RecallConfig,
    options: &CaptureAllOptions,
) -> Result<(CaptureAllReport, u8), CaptureAllSetupError> {
    let started = Instant::now();
    let root = resolve_runtime_root(config, options)?;
    let selected = select_sources(config, options.only_source.as_deref())?;
    let since_ms = match options.session_since.as_deref() {
        None => None,
        Some(text) => {
            let parsed = chrono::DateTime::parse_from_rfc3339(text.trim()).map_err(|_| {
                (
                    EXIT_USAGE,
                    format!("--session-since must be RFC3339 (e.g. 2026-10-23T00:00:00Z): {text}"),
                )
            })?;
            Some(parsed.timestamp_millis())
        }
    };

    let start_io = cost::sample_io().ok();
    if options.plan {
        let mut reports = Vec::new();
        for source in &selected {
            cost::begin_recording();
            let mut report = plan_source(source, since_ms);
            report.cost = cost::finish_recording().unwrap_or_default();
            reports.push(report);
        }
        // `load_ledger` only reads, so the plan reports the real ledger state.
        let (_, ledger_status) = load_ledger(Path::new(&root));
        let mut report = finalize_report(
            reports, &root, true, ledger_status,
            options.session_since.clone(), started,
        );
        report.cost = Cost::sample(
            started.elapsed().as_millis() as u64,
            start_io,
            cost::sample_io().ok(),
        );
        let code = if report.ok { 0 } else { EXIT_CAPTURE };
        return Ok((report, code));
    }

    let archive = Archive::open(Path::new(&root)).map_err(|error| (EXIT_IO, error.to_string()))?;
    if archive.lock_held() {
        if options.skip_if_locked {
            let (_, ledger_status) = load_ledger(Path::new(&root));
            let mut report =
                empty_report(&root, ledger_status, options.session_since.clone(), started);
            report.skipped = Some("locked".to_string());
            report.cost = Cost::sample(
                started.elapsed().as_millis() as u64,
                start_io,
                cost::sample_io().ok(),
            );
            return Ok((report, 0));
        }
        return Err((
            EXIT_LOCK,
            ArchiveError::LockHeld(Path::new(&root).join("locks").join("capture.lock")).to_string(),
        ));
    }

    let (mut ledger, ledger_status) = load_ledger(Path::new(&root));
    // One capture-all round: the record ledger and the source state are staged
    // in memory and written once, and the object-store reconcile runs once,
    // after every source finished (implementation-005, §G.4 cost assertions).
    let session = archive.begin_capture_session();
    let mut reports = Vec::new();
    for source in &selected {
        let previous = ledger
            .sources
            .remove(&source.source_id)
            .map(|entry| entry.files)
            .unwrap_or_default();
        cost::begin_recording();
        let (mut report, files) = run_source(&archive, source, &options.domain, since_ms, previous);
        report.cost = cost::finish_recording().unwrap_or_default();
        ledger.sources.insert(
            source.source_id.clone(),
            LedgerSource {
                provider: capture_provider(source.kind).unwrap_or_default().to_string(),
                files,
            },
        );
        reports.push(report);
    }
    // The ledger flush is the round's single write of records.json and
    // sources.json (records first, then source state; see
    // `Archive::begin_capture_session`). A failure there is a hard IO failure:
    // the run does not report success with an unwritten ledger.
    let (finished, flush_phase) = cost::timed(|| session.finish());
    finished.map_err(|error| (EXIT_IO, error.to_string()))?;
    ledger.updated_at_ms = now_ms();
    ledger.version = LEDGER_VERSION;
    save_ledger(Path::new(&root), &ledger)
        .map_err(|error| (EXIT_IO, error.to_string()))?;

    let mut report = finalize_report(
        reports,
        &root,
        false,
        ledger_status,
        options.session_since.clone(),
        started,
    );
    // One reconcile per round: the object directory is walked once, against the
    // committed manifests (all of them, including this round's). A failed
    // reconcile never turns into a silent success.
    let (reconcile, reconcile_phase) = cost::timed(|| archive.reconcile(&[]));
    let mut reconcile_error = None;
    match reconcile {
        Ok(reconcile) => report.reconcile = Some(reconcile),
        Err(error) => {
            report.ok = false;
            reconcile_error = Some(error.to_string());
        }
    }
    report.reconcile_error = reconcile_error;
    report.ledgers = archive.ledger_stats();
    report.records_compact_error = archive.records_compact_error();
    report.cost = Cost::sample(
        started.elapsed().as_millis() as u64,
        start_io,
        cost::sample_io().ok(),
    );
    // Round-level stages and counters: no recorder is active between the
    // per-source recordings, so the round report carries them itself.
    report
        .cost
        .phases
        .insert("ledger_flush".to_string(), flush_phase);
    report.cost.phases.insert("reconcile".to_string(), reconcile_phase);
    // These counters go in **after** `Cost::sample` replaced the map: inserting
    // them before it would drop them again. The reconcile counts are the same
    // numbers as the `report.reconcile` block, in the machine-consumed shape the
    // README documents.
    //
    // `records_read` / `sources_read` still count where the load happened (one
    // source), the writes happen after the last source and are only in
    // `report.ledgers`; duplicating them here would double count a summed
    // `cost.counters` map.
    report
        .cost
        .counters
        .insert("reconcile.calls".to_string(), 1);
    if let Some(reconcile) = report.reconcile.as_ref() {
        report.cost.counters.insert(
            "reconcile.objects_on_disk".to_string(),
            reconcile.objects_on_disk,
        );
        report.cost.counters.insert(
            "reconcile.objects_referenced".to_string(),
            reconcile.objects_referenced,
        );
        report.cost.counters.insert(
            "reconcile.orphan_objects".to_string(),
            reconcile.orphan_objects.len() as u64,
        );
    }
    let code = if report.ok { 0 } else { EXIT_CAPTURE };
    Ok((report, code))
}

fn empty_report(
    root: &str,
    ledger: LedgerStatus,
    session_since: Option<String>,
    started: Instant,
) -> CaptureAllReport {
    CaptureAllReport {
        ok: true,
        plan: false,
        root: root.to_string(),
        skipped: None,
        ledger: ledger.as_str().to_string(),
        session_since,
        sources: Vec::new(),
        totals: TotalsReport {
            files_total: 0,
            files_captured: 0,
            files_skipped_unchanged: 0,
            files_skipped_since: 0,
            files_failed: 0,
            records_added: 0,
            bytes_added: 0,
            errors: 0,
        },
        duration_ms: started.elapsed().as_millis() as u64,
        cost: Cost::default(),
        reconcile: None,
        reconcile_error: None,
        ledgers: LedgerStats::default(),
        records_compact_error: None,
    }
}

fn finalize_report(
    sources: Vec<SourceReport>,
    root: &str,
    plan: bool,
    ledger: LedgerStatus,
    session_since: Option<String>,
    started: Instant,
) -> CaptureAllReport {
    let mut totals = TotalsReport {
        files_total: 0,
        files_captured: 0,
        files_skipped_unchanged: 0,
        files_skipped_since: 0,
        files_failed: 0,
        records_added: 0,
        bytes_added: 0,
        errors: 0,
    };
    let mut ok = true;
    for source in &sources {
        totals.files_total += source.files_total;
        totals.files_captured += source.files_captured;
        totals.files_skipped_unchanged += source.files_skipped_unchanged;
        totals.files_skipped_since += source.files_skipped_since;
        totals.files_failed += source.files_failed;
        totals.records_added += source.records_added;
        totals.bytes_added += source.bytes_added;
        totals.errors += source.errors.len() as u64;
        if !source.errors.is_empty() {
            ok = false;
        }
    }
    CaptureAllReport {
        ok,
        plan,
        root: root.to_string(),
        skipped: None,
        ledger: ledger.as_str().to_string(),
        session_since,
        sources,
        totals,
        duration_ms: started.elapsed().as_millis() as u64,
        cost: Cost::default(),
        reconcile: None,
        reconcile_error: None,
        ledgers: LedgerStats::default(),
        records_compact_error: None,
    }
}

/// `--plan`: enumerate and stat only; nothing is opened for writing.
fn plan_source(source: &SourceConfig, since_ms: Option<i64>) -> SourceReport {
    let started = Instant::now();
    let enumerated = enumerate_source(source);
    let dsh = plan_dsh_report(&enumerated);
    let mut files_skipped_since = 0u64;
    let mut bytes_total = 0u64;
    let mut errors = enumerated.errors;
    for path in &enumerated.files {
        let Some((size, mtime_ms)) = file_stamp(path) else {
            push_enum_error(&mut errors, path, CODE_STAT_IO);
            continue;
        };
        if let Some(since) = since_ms {
            if mtime_ms < since {
                files_skipped_since += 1;
                continue;
            }
        }
        bytes_total += size;
    }
    SourceReport {
        source_id: source.source_id.clone(),
        kind: source.kind.as_str().to_string(),
        provider: capture_provider(source.kind).unwrap_or_default().to_string(),
        root: source.root.clone(),
        files_total: enumerated.files.len() as u64,
        files_captured: 0,
        files_skipped_unchanged: 0,
        files_skipped_since,
        files_failed: 0,
        records_added: 0,
        bytes_added: 0,
        bytes_total,
        unsupported_option: Vec::new(),
        errors,
        dsh,
        changes: ChangeCounts::default(),
        duration_ms: started.elapsed().as_millis() as u64,
        cost: Cost::default(),
    }
}

/// The static half of the DSH report (what enumeration alone can say).
fn plan_dsh_report(enumerated: &Enumerated) -> Option<DshReport> {
    let inventory = enumerated.dsh.as_ref()?;
    let report = DshReport {
        sessions_total: inventory.sessions.len() as u64,
        sessions_unsupported: inventory.unsupported.len() as u64,
        unsupported: inventory.unsupported.clone(),
        ..DshReport::default()
    };
    Some(report)
}

/// Capture one configured source file by file. Returns the per-source report
/// and the rebuilt ledger map for this source (only successes are recorded, so
/// a failed file is retried on the next run).
fn run_source(
    archive: &Archive,
    source: &SourceConfig,
    domain: &str,
    since_ms: Option<i64>,
    previous: BTreeMap<String, LedgerFile>,
) -> (SourceReport, BTreeMap<String, LedgerFile>) {
    let started = Instant::now();
    let provider = capture_provider(source.kind).unwrap_or_default().to_string();
    let enumerated = cost::measure("enumerate", || enumerate_source(source));
    if source.kind == SourceKind::Dsh {
        return run_dsh_source(
            archive,
            source,
            domain,
            since_ms,
            previous,
            enumerated,
            started,
        );
    }
    let mut errors = enumerated.errors;
    let mut rebuilt: BTreeMap<String, LedgerFile> = BTreeMap::new();
    let mut files_captured = 0u64;
    let mut files_skipped_unchanged = 0u64;
    let mut files_skipped_since = 0u64;
    let mut files_failed = 0u64;
    let mut records_added = 0u64;
    let mut bytes_added = 0u64;
    let mut changes = ChangeCounts::default();
    // Every option is supported by the engine; the field stays for options a
    // future source kind cannot honour.
    let unsupported_option = Vec::new();

    for path in &enumerated.files {
        let key = canonical_key(path);
        let sqlite_entry = enumerated.has_sqlite && is_sqlite_db_path(path);
        let Some((size, mtime_ms)) = file_stamp(path) else {
            files_failed += 1;
            push_enum_error(&mut errors, path, CODE_STAT_IO);
            continue;
        };
        // The SQLite source gets the time filter as a capture option (the
        // engine filters sessions by their own update time); only file
        // sources are skipped by mtime.
        if let Some(since_ms) = since_ms {
            if !sqlite_entry && mtime_ms < since_ms {
                files_skipped_since += 1;
                changes.skipped_since += 1;
                if let Some(entry) = previous.get(&key) {
                    rebuilt.insert(key, entry.clone());
                }
                continue;
            }
        }
        // SQLite evidence is periodic observation: the ledger never skips it.
        if !sqlite_entry {
            if let Some(entry) = previous.get(&key) {
                if entry.size == size && entry.mtime_ms == mtime_ms {
                    files_skipped_unchanged += 1;
                    changes.skipped_unchanged += 1;
                    rebuilt.insert(key, entry.clone());
                    continue;
                }
            }
        }
        let options = CaptureOptions {
            session_since_ms: if sqlite_entry { since_ms } else { None },
            ..CaptureOptions::default()
        };
        match capture_source_with_options(archive, path, &provider, domain, None, options) {
            Ok(report) if report.ok => {
                changes.observe(&report);
                files_captured += 1;
                records_added += report.records_new;
                bytes_added += report.bytes;
                if report.status == "source_missing" {
                    // The file vanished before capture: drop any stale entry.
                } else {
                    rebuilt.insert(
                        key,
                        LedgerFile {
                            size,
                            mtime_ms,
                            capture_id: report.capture_id.clone(),
                            captured_at_ms: Some(now_ms()),
                            dsh_rel_path: None,
                            dsh_lines: None,
                            dsh_last_line_sha256: None,
                            dsh_previous: None,
                        },
                    );
                }
            }
            Ok(report) => {
                changes.observe(&report);
                files_failed += 1;
                errors.push(SourceError {
                    path: path.to_string_lossy().to_string(),
                    code: report.status.clone(),
                });
                if let Some(entry) = previous.get(&key) {
                    rebuilt.insert(key, entry.clone());
                }
            }
            Err(error) => {
                changes.failed += 1;
                files_failed += 1;
                errors.push(SourceError {
                    path: path.to_string_lossy().to_string(),
                    code: error_code(&error).to_string(),
                });
                if let Some(entry) = previous.get(&key) {
                    rebuilt.insert(key, entry.clone());
                }
            }
        }
    }

    let report = SourceReport {
        source_id: source.source_id.clone(),
        kind: source.kind.as_str().to_string(),
        provider,
        root: source.root.clone(),
        files_total: enumerated.files.len() as u64,
        files_captured,
        files_skipped_unchanged,
        files_skipped_since,
        files_failed,
        records_added,
        bytes_added,
        bytes_total: 0,
        unsupported_option,
        errors,
        dsh: None,
        changes,
        duration_ms: started.elapsed().as_millis() as u64,
        cost: Cost::default(),
    };
    (report, rebuilt)
}

/// Capture every enumerated DSH carrier.
///
/// Enumeration already selected one carrier per session directory (the highest
/// generation the reader understands) and listed the directories whose highest
/// generation is unknown. The ledger keeps the usual size/mtime skip; inside the
/// engine the DSH checkpoint decides append versus recapture.
fn run_dsh_source(
    archive: &Archive,
    source: &SourceConfig,
    domain: &str,
    since_ms: Option<i64>,
    previous: BTreeMap<String, LedgerFile>,
    enumerated: Enumerated,
    started: Instant,
) -> (SourceReport, BTreeMap<String, LedgerFile>) {
    let provider = capture_provider(source.kind).unwrap_or_default().to_string();
    let mut dsh_report = plan_dsh_report(&enumerated).unwrap_or_default();
    let inventory = enumerated.dsh.clone().unwrap_or_default();
    let mut errors = enumerated.errors;

    let mut rebuilt: BTreeMap<String, LedgerFile> = BTreeMap::new();
    let mut files_captured = 0u64;
    let mut files_skipped_unchanged = 0u64;
    let mut files_skipped_since = 0u64;
    let mut files_failed = 0u64;
    let mut records_added = 0u64;
    let mut bytes_added = 0u64;
    let mut changes = ChangeCounts::default();

    for session in &inventory.sessions {
        let path = &session.path;
        // One stable identity per session: the directory, not the generation
        // file (which changes on a DSH format migration).
        let key = capture_dsh::source_key(path);
        let Some((size, mtime_ms)) = file_stamp(path) else {
            files_failed += 1;
            push_enum_error(&mut errors, path, CODE_STAT_IO);
            continue;
        };
        if let Some(since) = since_ms {
            if mtime_ms < since {
                files_skipped_since += 1;
                changes.skipped_since += 1;
                if let Some(entry) = previous.get(&key) {
                    rebuilt.insert(key, entry.clone());
                }
                continue;
            }
        }
        if let Some(entry) = previous.get(&key) {
            if entry.size == size && entry.mtime_ms == mtime_ms {
                files_skipped_unchanged += 1;
                changes.skipped_unchanged += 1;
                rebuilt.insert(key, entry.clone());
                continue;
            }
        }
        match capture_dsh::capture_dsh_source(archive, path, &session.rel_path, &provider, domain, None) {
            Ok((report, facts)) if report.ok => {
                changes.observe(&report);
                files_captured += 1;
                records_added += report.records_new;
                bytes_added += report.bytes;
                dsh_report.malformed_lines += facts.malformed_lines;
                if facts.partial_tail {
                    dsh_report.partial_tail += 1;
                }
                if facts.corrupt {
                    dsh_report.corrupt += 1;
                }
                if facts.truncated {
                    dsh_report.truncated += 1;
                }
                let link = facts
                    .change_reason
                    .as_deref()
                    .filter(|reason| {
                        matches!(*reason, "generation_switch" | "last_line_rewritten" | "prefix_rewritten" | "logical_lines_shrunk")
                    })
                    .map(|reason| DshLink {
                        rel_path: facts.rel_path.clone(),
                        previous_rel_path: facts.previous_rel_path.clone(),
                        reason: reason.to_string(),
                    });
                if let Some(link) = &link {
                    dsh_report.links.push(link.clone());
                }
                // A link recorded by an earlier restart still describes this
                // source while the same carrier stays active: keep it instead of
                // forgetting the migration on the next append.
                let previous_link = link.clone().or_else(|| {
                    previous.get(&key).and_then(|entry| {
                        if entry.dsh_rel_path.as_deref()
                            == Some(facts.rel_path.as_str())
                        {
                            entry.dsh_previous.clone()
                        } else {
                            None
                        }
                    })
                });
                rebuilt.insert(
                    key,
                    LedgerFile {
                        size,
                        mtime_ms,
                        capture_id: report.capture_id.clone(),
                        captured_at_ms: Some(now_ms()),
                        dsh_rel_path: Some(facts.rel_path.clone()),
                        dsh_lines: Some(facts.logical_lines),
                        dsh_last_line_sha256: Some(facts.last_line_sha256.clone()),
                        dsh_previous: previous_link,
                    },
                );
            }
            Ok((report, facts)) => {
                changes.observe(&report);
                files_failed += 1;
                dsh_report.malformed_lines += facts.malformed_lines;
                if facts.partial_tail {
                    dsh_report.partial_tail += 1;
                }
                if facts.corrupt {
                    dsh_report.corrupt += 1;
                }
                if facts.truncated {
                    dsh_report.truncated += 1;
                }
                errors.push(SourceError {
                    path: path.to_string_lossy().to_string(),
                    code: report.status.clone(),
                });
                if let Some(entry) = previous.get(&key) {
                    rebuilt.insert(key, entry.clone());
                }
            }
            Err(error) => {
                changes.failed += 1;
                files_failed += 1;
                errors.push(SourceError {
                    path: path.to_string_lossy().to_string(),
                    code: error_code(&error).to_string(),
                });
                if let Some(entry) = previous.get(&key) {
                    rebuilt.insert(key, entry.clone());
                }
            }
        }
    }

    let report = SourceReport {
        source_id: source.source_id.clone(),
        kind: source.kind.as_str().to_string(),
        provider,
        root: source.root.clone(),
        files_total: inventory.sessions.len() as u64,
        files_captured,
        files_skipped_unchanged,
        files_skipped_since,
        files_failed,
        records_added,
        bytes_added,
        bytes_total: 0,
        unsupported_option: Vec::new(),
        errors,
        dsh: Some(dsh_report),
        changes,
        duration_ms: started.elapsed().as_millis() as u64,
        cost: Cost::default(),
    };
    (report, rebuilt)
}
