//! P5 coverage report: provider aggregation, source categories, observation
//! windows and framework-baseline reconciliation.
//!
//! This module is a projection layer only. Capture facts come from committed
//! manifests + collector state; derived status comes from the checkpoint index
//! (`derived/state.json`); integrity status comes from the scrub ledger
//! (`state/scrub.json`). No value here is written back into the archive.
//!
//! # Baseline reconciliation
//!
//! A framework baseline is an *external* count that can disagree with the
//! archive. Three kinds are supported, each with an explicit comparison unit:
//!
//! | kind | baseline unit | archive unit |
//! |---|---|---|
//! | `codex_session_index` | distinct `id` in `session_index.jsonl` | session id inferred from covered `rollout-*` file names |
//! | `opencode_session_table` | session ids in the live DB (`session_v2` / `session`) | session ids in the latest archived observation |
//! | `directory_enumeration` | matching files under a root | covered source paths under that root |
//!
//! A disagreement is reported as `mismatch` with the exact missing/extra entries
//! and makes the report `ok = false`. An unreadable baseline is `unavailable`,
//! never silently treated as empty.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::archive::Archive;
use crate::archive::ArchiveError;
use crate::coverage::{CoverageReport, SourceCoverage};
use crate::replay::{checkpoint_snapshot, ReplayIdentity};

pub const CATEGORY_COVERED: &str = "covered";
pub const CATEGORY_DISABLED: &str = "disabled";
pub const CATEGORY_UNSUPPORTED: &str = "unsupported";
pub const CATEGORY_READ_FAILED: &str = "read_failed";
pub const CATEGORY_PARSE_FAILED: &str = "parse_failed";
pub const CATEGORY_ATTACHMENT_MISSING: &str = "attachment_missing";
pub const CATEGORY_DERIVED_LAG: &str = "derived_lag";
pub const CATEGORY_NOT_CAPTURED: &str = "not_captured";
pub const CATEGORY_CORRUPTION: &str = "corruption";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportConfig {
    pub version: u32,
    #[serde(default)]
    pub sources: Vec<DeclaredSource>,
}

impl Default for ReportConfig {
    fn default() -> Self {
        Self {
            version: 1,
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredSource {
    pub id: String,
    pub provider: String,
    /// File or directory root used for matching archived sources. Absent means
    /// "every archived source of this provider".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Declared format generation, checked against the P0 support matrix. An
    /// unknown or unsupported generation is explicitly reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_generation: Option<String>,
    /// Access domain used for archive reads (SQLite observation replay). Empty
    /// means fail-closed: a baseline that needs bytes cannot be evaluated.
    #[serde(default)]
    pub access: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineSpec {
    /// `codex_session_index` | `opencode_session_table` | `directory_enumeration`
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// File-name glob for `directory_enumeration` (e.g. `rollout-*.jsonl`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default = "default_true")]
    pub recursive: bool,
}

fn default_true() -> bool {
    true
}

impl ReportConfig {
    /// Load a config file. `Ok(None)` when the file does not exist, so an
    /// explicitly injected root without `config.local.json` reports a
    /// provider/source-only view instead of failing.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let config: Self = serde_json::from_slice(&raw)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if config.version != 1 {
            return Err(format!(
                "{}: unsupported config version {}",
                path.display(),
                config.version
            ));
        }
        Ok(Some(config))
    }
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CategoryTotals {
    pub covered: u64,
    pub disabled: u64,
    pub unsupported: u64,
    pub read_failed: u64,
    pub parse_failed: u64,
    pub attachment_missing: u64,
    pub derived_lag: u64,
    pub not_captured: u64,
    pub corruption: u64,
}

impl CategoryTotals {
    pub fn count(&mut self, category: &str) {
        match category {
            CATEGORY_COVERED => self.covered += 1,
            CATEGORY_DISABLED => self.disabled += 1,
            CATEGORY_UNSUPPORTED => self.unsupported += 1,
            CATEGORY_READ_FAILED => self.read_failed += 1,
            CATEGORY_PARSE_FAILED => self.parse_failed += 1,
            CATEGORY_ATTACHMENT_MISSING => self.attachment_missing += 1,
            CATEGORY_DERIVED_LAG => self.derived_lag += 1,
            CATEGORY_NOT_CAPTURED => self.not_captured += 1,
            CATEGORY_CORRUPTION => self.corruption += 1,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderReport {
    pub provider: String,
    pub enabled: bool,
    pub declared_sources: u64,
    pub archived_sources: u64,
    pub covered_sources: u64,
    pub disabled_sources: u64,
    pub unsupported_sources: u64,
    pub read_failed_sources: u64,
    pub parse_failed_sources: u64,
    pub attachment_missing_sources: u64,
    pub derived_lagging_sources: u64,
    pub discovered: u64,
    pub captured: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineReport {
    pub kind: String,
    pub provider: String,
    pub source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub status: String,
    pub baseline_count: u64,
    pub archive_count: u64,
    pub missing_from_archive: Vec<String>,
    pub extra_in_archive: Vec<String>,
    #[serde(default)]
    pub anomalies: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconciliationReport {
    pub ok: bool,
    pub baselines: Vec<BaselineReport>,
    #[serde(default)]
    pub anomalies: Vec<String>,
}

impl Default for ReconciliationReport {
    fn default() -> Self {
        Self {
            ok: true,
            baselines: Vec::new(),
            anomalies: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedReport {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub status: String,
    pub parser_version: String,
    pub model_version: u32,
    pub sources_ok: u64,
    pub sources_lagging: u64,
    pub sources_failed: u64,
    #[serde(default)]
    pub anomalies: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationWindow {
    /// `records` (file-backed source) or `sqlite_observation`.
    pub kind: String,
    pub watermark_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<i64>,
    pub records: u64,
    pub offset: u64,
    pub line: u64,
    pub observation_seq: u64,
    /// What this window does not prove. Always present.
    pub blind_window: String,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn normalized(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let raw = raw.trim_end_matches('/');
    let raw = if raw.is_empty() { "/" } else { raw };
    #[cfg(windows)]
    {
        // Windows paths compare case-insensitively.
        return raw.to_lowercase();
    }
    #[cfg(not(windows))]
    {
        raw.to_string()
    }
}

fn under_root(source: &str, root: &str) -> bool {
    let source = normalized(Path::new(source));
    let root = normalized(Path::new(root));
    if root == "/" {
        return source.starts_with('/');
    }
    source == root
        || source
            .strip_prefix(&root)
            .map(|rest| rest.starts_with('/'))
            .unwrap_or(false)
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern = if cfg!(windows) {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    let name = if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_string()
    };
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(star_pi) = star {
            pi = star_pi + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn collect_files(root: &Path, recursive: bool, pattern: Option<&str>, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if recursive {
                collect_files(&path, recursive, pattern, out);
            }
            continue;
        }
        let name = file_name(&path.to_string_lossy());
        if pattern.map(|pattern| glob_match(pattern, &name)).unwrap_or(true) {
            out.push(path);
        }
    }
}

fn inferred_session_id(path: &str) -> Option<String> {
    let name = file_name(path);
    let bytes: Vec<char> = name.chars().collect();
    let is_hex = |ch: char| ch.is_ascii_hexdigit();
    for start in 0..bytes.len() {
        // 8-4-4-4-12 UUID.
        if start + 36 > bytes.len() {
            break;
        }
        let candidate = &bytes[start..start + 36];
        if candidate[8] == '-'
            && candidate[13] == '-'
            && candidate[18] == '-'
            && candidate[23] == '-'
            && candidate
                .iter()
                .enumerate()
                .all(|(index, ch)| [8usize, 13, 18, 23].contains(&index) || is_hex(*ch))
        {
            return Some(candidate.iter().collect::<String>().to_lowercase());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Baseline evaluation
// ---------------------------------------------------------------------------

fn declared_matches(declared: &DeclaredSource, source: &str, provider: &str) -> bool {
    if declared.provider != provider {
        return false;
    }
    match declared.root.as_deref() {
        Some(root) => under_root(source, root),
        None => true,
    }
}

fn source_rows_for<'a>(
    report: &'a CoverageReport,
    declared: &DeclaredSource,
) -> Vec<&'a SourceCoverage> {
    report
        .sources
        .iter()
        .filter(|row| declared_matches(declared, &row.source, &row.provider))
        .collect()
}

/// A row that actually represents committed evidence (not a synthetic
/// `not_captured`/`disabled` declaration and not a never-captured failure).
fn evidence_row(row: &SourceCoverage) -> bool {
    row.records_committed > 0
        || row.bytes_total > 0
        || !row.captures.is_empty()
        || row
            .sqlite
            .as_ref()
            .map(|sqlite| sqlite.observation_seq > 0)
            .unwrap_or(false)
}

fn baseline_unavailable(
    declared: &DeclaredSource,
    spec: &BaselineSpec,
    error: String,
) -> BaselineReport {
    BaselineReport {
        kind: spec.kind.clone(),
        provider: declared.provider.clone(),
        source_id: declared.id.clone(),
        path: spec.path.clone(),
        status: "unavailable".to_string(),
        baseline_count: 0,
        archive_count: 0,
        missing_from_archive: Vec::new(),
        extra_in_archive: Vec::new(),
        anomalies: vec![error],
    }
}

fn evaluate_directory_baseline(
    declared: &DeclaredSource,
    spec: &BaselineSpec,
    report: &CoverageReport,
) -> BaselineReport {
    let Some(path) = spec.path.as_deref() else {
        return baseline_unavailable(declared, spec, "directory baseline has no path".to_string());
    };
    let root = Path::new(path);
    if !root.exists() {
        return baseline_unavailable(
            declared,
            spec,
            format!("baseline directory {path} is unavailable"),
        );
    }
    let mut files = Vec::new();
    collect_files(root, spec.recursive, spec.pattern.as_deref(), &mut files);
    let baseline: BTreeSet<String> = files
        .iter()
        .map(|file| normalized(file))
        .collect();
    let archive: BTreeSet<String> = source_rows_for(report, declared)
        .into_iter()
        .filter(|row| evidence_row(row))
        .map(|row| normalized(Path::new(&row.source)))
        .filter(|source| {
            spec.pattern
                .as_deref()
                .map(|pattern| glob_match(pattern, &file_name(source)))
                .unwrap_or(true)
        })
        .collect();
    let missing_from_archive: Vec<String> = baseline.difference(&archive).cloned().collect();
    let extra_in_archive: Vec<String> = archive.difference(&baseline).cloned().collect();
    let ok = missing_from_archive.is_empty() && extra_in_archive.is_empty();
    let mut anomalies = Vec::new();
    if !ok {
        anomalies.push(format!(
            "directory enumeration found {} file(s), archive covers {} (missing {}, extra {})",
            baseline.len(),
            archive.len(),
            missing_from_archive.len(),
            extra_in_archive.len()
        ));
    }
    BaselineReport {
        kind: spec.kind.clone(),
        provider: declared.provider.clone(),
        source_id: declared.id.clone(),
        path: spec.path.clone(),
        status: if ok { "ok" } else { "mismatch" }.to_string(),
        baseline_count: baseline.len() as u64,
        archive_count: archive.len() as u64,
        missing_from_archive,
        extra_in_archive,
        anomalies,
    }
}

fn evaluate_codex_index_baseline(
    declared: &DeclaredSource,
    spec: &BaselineSpec,
    report: &CoverageReport,
) -> BaselineReport {
    let Some(path) = spec.path.as_deref() else {
        return baseline_unavailable(declared, spec, "codex baseline has no path".to_string());
    };
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            return baseline_unavailable(
                declared,
                spec,
                format!("baseline {path} is unavailable: {error}"),
            )
        }
    };
    let mut baseline = BTreeSet::new();
    for line in raw.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            let id = id.trim();
            if !id.is_empty() {
                baseline.insert(id.to_lowercase());
            }
        }
    }
    let mut archive = BTreeSet::new();
    for row in source_rows_for(report, declared) {
        if !evidence_row(row) {
            continue;
        }
        if let Some(id) = inferred_session_id(&row.source) {
            archive.insert(id);
        }
    }
    let missing_from_archive: Vec<String> = baseline.difference(&archive).cloned().collect();
    let extra_in_archive: Vec<String> = archive.difference(&baseline).cloned().collect();
    let ok = missing_from_archive.is_empty() && extra_in_archive.is_empty();
    let mut anomalies = Vec::new();
    if !ok {
        anomalies.push(format!(
            "session_index lists {} thread(s), archived rollout evidence names {} (missing {}, extra {})",
            baseline.len(),
            archive.len(),
            missing_from_archive.len(),
            extra_in_archive.len()
        ));
    }
    BaselineReport {
        kind: spec.kind.clone(),
        provider: declared.provider.clone(),
        source_id: declared.id.clone(),
        path: spec.path.clone(),
        status: if ok { "ok" } else { "mismatch" }.to_string(),
        baseline_count: baseline.len() as u64,
        archive_count: archive.len() as u64,
        missing_from_archive,
        extra_in_archive,
        anomalies,
    }
}

fn evaluate_opencode_table_baseline(
    archive: &Archive,
    declared: &DeclaredSource,
    spec: &BaselineSpec,
    report: &CoverageReport,
) -> Result<BaselineReport, ArchiveError> {
    let Some(path) = spec.path.as_deref() else {
        return Ok(baseline_unavailable(
            declared,
            spec,
            "opencode baseline has no path".to_string(),
        ));
    };
    let mut baseline = BTreeSet::new();
    match fs::metadata(path) {
        Ok(_) => {}
        Err(error) => {
            return Ok(baseline_unavailable(
                declared,
                spec,
                format!("baseline database {path} is unavailable: {error}"),
            ))
        }
    }
    let conn = match rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(error) => {
            return Ok(baseline_unavailable(
                declared,
                spec,
                format!("baseline database {path} cannot be opened read-only: {error}"),
            ))
        }
    };
    let tables: Vec<&str> = ["session_v2", "session"]
        .into_iter()
        .filter(|table| table_exists(&conn, table))
        .collect();
    if tables.is_empty() {
        return Ok(baseline_unavailable(
            declared,
            spec,
            format!("baseline database {path} has no session table"),
        ));
    }
    for table in &tables {
        let mut statement = match conn.prepare(&format!("SELECT id FROM \"{table}\"")) {
            Ok(statement) => statement,
            Err(error) => {
                return Ok(baseline_unavailable(
                    declared,
                    spec,
                    format!("baseline session table {table} is unreadable: {error}"),
                ))
            }
        };
        let ids: Result<Vec<String>, rusqlite::Error> = {
            let rows = statement.query_map([], |row| row.get::<_, String>(0));
            match rows {
                Ok(rows) => rows.flatten().map(Ok).collect(),
                Err(error) => Err(error),
            }
        };
        match ids {
            Ok(ids) => {
                for id in ids {
                    baseline.insert(id);
                }
            }
            Err(error) => {
                return Ok(baseline_unavailable(
                    declared,
                    spec,
                    format!("baseline session table {table} is unreadable: {error}"),
                ))
            }
        }
    }
    drop(conn);

    let domain = declared.access.first().cloned().unwrap_or_default();
    if domain.is_empty() {
        return Ok(baseline_unavailable(
            declared,
            spec,
            "declared source has no access mapping; archived rows cannot be read (fail-closed)"
                .to_string(),
        ));
    }
    let mut archive_ids = BTreeSet::new();
    let mut anomalies = Vec::new();
    let mut replay_failed = false;
    for row in source_rows_for(report, declared) {
        if !evidence_row(row) {
            continue;
        }
        match crate::capture_sqlite::replay_sqlite(archive, &row.source, None, &domain) {
            Ok(replay) => {
                let mut found_table = false;
                for table in ["session_v2", "session"] {
                    if let Some(rows) = replay.tables.get(table) {
                        found_table = true;
                        for value in rows.values() {
                            if let Some(id) = value.get("id").and_then(Value::as_str) {
                                archive_ids.insert(id.to_string());
                            }
                        }
                    }
                }
                if !found_table {
                    replay_failed = true;
                    anomalies.push(format!(
                        "{}: archived observation has no session table",
                        row.source
                    ));
                }
            }
            Err(error) => {
                replay_failed = true;
                anomalies.push(format!(
                    "{}: archived observation cannot be replayed: {error}",
                    row.source
                ));
            }
        }
    }
    if replay_failed {
        return Ok(BaselineReport {
            kind: spec.kind.clone(),
            provider: declared.provider.clone(),
            source_id: declared.id.clone(),
            path: spec.path.clone(),
            status: "unavailable".to_string(),
            baseline_count: baseline.len() as u64,
            archive_count: archive_ids.len() as u64,
            missing_from_archive: Vec::new(),
            extra_in_archive: Vec::new(),
            anomalies,
        });
    }
    let missing_from_archive: Vec<String> = baseline.difference(&archive_ids).cloned().collect();
    let extra_in_archive: Vec<String> = archive_ids.difference(&baseline).cloned().collect();
    let ok = missing_from_archive.is_empty() && extra_in_archive.is_empty();
    if !ok {
        anomalies.push(format!(
            "live session table has {} session(s), archived observation has {} (missing {}, extra {})",
            baseline.len(),
            archive_ids.len(),
            missing_from_archive.len(),
            extra_in_archive.len()
        ));
    }
    Ok(BaselineReport {
        kind: spec.kind.clone(),
        provider: declared.provider.clone(),
        source_id: declared.id.clone(),
        path: spec.path.clone(),
        status: if ok { "ok" } else { "mismatch" }.to_string(),
        baseline_count: baseline.len() as u64,
        archive_count: archive_ids.len() as u64,
        missing_from_archive,
        extra_in_archive,
        anomalies,
    })
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
    .unwrap_or(false)
}

fn evaluate_baseline(
    archive: &Archive,
    declared: &DeclaredSource,
    spec: &BaselineSpec,
    report: &CoverageReport,
) -> Result<BaselineReport, ArchiveError> {
    if !declared.enabled {
        return Ok(BaselineReport {
            kind: spec.kind.clone(),
            provider: declared.provider.clone(),
            source_id: declared.id.clone(),
            path: spec.path.clone(),
            status: "disabled".to_string(),
            baseline_count: 0,
            archive_count: 0,
            missing_from_archive: Vec::new(),
            extra_in_archive: Vec::new(),
            anomalies: Vec::new(),
        });
    }
    match spec.kind.as_str() {
        "directory_enumeration" => Ok(evaluate_directory_baseline(declared, spec, report)),
        "codex_session_index" => Ok(evaluate_codex_index_baseline(declared, spec, report)),
        "opencode_session_table" => {
            evaluate_opencode_table_baseline(archive, declared, spec, report)
        }
        other => Ok(baseline_unavailable(
            declared,
            spec,
            format!("unsupported baseline kind {other:?}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Report evaluation
// ---------------------------------------------------------------------------

fn add_category(row: &mut SourceCoverage, category: &str) {
    if !row.categories.iter().any(|value| value == category) {
        row.categories.push(category.to_string());
    }
}

fn synthetic_row(
    declared: &DeclaredSource,
    status: &str,
    category: &str,
) -> SourceCoverage {
    let mut row = SourceCoverage::default();
    row.source = declared
        .root
        .clone()
        .unwrap_or_else(|| format!("declared:{}", declared.id));
    row.provider = declared.provider.clone();
    row.status = status.to_string();
    row.configured = true;
    row.category = category.to_string();
    row.categories = vec![category.to_string()];
    row
}

/// Apply declarations, derived status, scrub ledger and baselines to a base
/// coverage projection.
pub fn evaluate(
    archive: &Archive,
    config: &ReportConfig,
    report: &mut CoverageReport,
) -> Result<(), ArchiveError> {
    // 1. Declarations claim matching archive rows; disabled declarations are
    //    still visible (explicit state, never omitted).
    for declared in &config.sources {
        let matches: Vec<usize> = report
            .sources
            .iter()
            .enumerate()
            .filter(|(_, row)| declared_matches(declared, &row.source, &row.provider))
            .map(|(index, _)| index)
            .collect();
        let supported = format_generation_supported(declared);
        if matches.is_empty() {
            let (status, category) = if !declared.enabled {
                ("disabled", CATEGORY_DISABLED)
            } else if supported != Ok(()) {
                ("unsupported", CATEGORY_UNSUPPORTED)
            } else {
                ("not_captured", CATEGORY_NOT_CAPTURED)
            };
            let mut row = synthetic_row(declared, status, category);
            if let Err(note) = supported {
                row.categories.push(CATEGORY_UNSUPPORTED.to_string());
                row.category = CATEGORY_UNSUPPORTED.to_string();
                report.anomalies.push(format!(
                    "declared source {}: {note}",
                    declared.id
                ));
            }
            report.sources.push(row);
            continue;
        }
        for index in matches {
            let row = &mut report.sources[index];
            row.configured = true;
            if !declared.enabled {
                add_category(row, CATEGORY_DISABLED);
                row.category = CATEGORY_DISABLED.to_string();
                row.status = "disabled".to_string();
                continue;
            }
            if let Err(note) = format_generation_supported(declared) {
                add_category(row, CATEGORY_UNSUPPORTED);
                row.category = CATEGORY_UNSUPPORTED.to_string();
                report.anomalies.push(format!(
                    "{}: {note}",
                    row.source
                ));
            }
        }
    }

    // 2. Derived checkpoint status (lag is a state, not an archive failure).
    //    A corrupt/unreadable derived index is reported as an anomaly; it is a
    //    rebuildable artifact and must not hide raw capture coverage.
    let identity = ReplayIdentity::current();
    let mut derived_state_anomalies = Vec::new();
    let snapshot = match checkpoint_snapshot(archive) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report.ok = false;
            derived_state_anomalies.push(format!(
                "derived checkpoint index is unreadable: {error}"
            ));
            crate::replay::CheckpointSnapshot {
                generation: None,
                status: "corrupt".to_string(),
                entries: BTreeMap::new(),
                anomalies: Vec::new(),
            }
        }
    };
    let mut derived_sources_ok = 0u64;
    let mut derived_sources_failed = 0u64;
    for row in report.sources.iter_mut() {
        if row.categories.iter().any(|category| {
            matches!(
                category.as_str(),
                CATEGORY_DISABLED | CATEGORY_UNSUPPORTED | CATEGORY_NOT_CAPTURED
            )
        }) {
            // Excluded sources are not expected to have a derived checkpoint.
            continue;
        }
        let last_capture_ms = row.captures.last().map(|capture| capture.captured_at_ms);
        let entry = snapshot.entries.get(&row.source);
        let lagging = match entry {
            None => {
                row.records_committed > 0
                    || row.bytes_total > 0
                    || row
                        .sqlite
                        .as_ref()
                        .map(|sqlite| sqlite.observation_seq > 0)
                        .unwrap_or(false)
            }
            Some(entry) => {
                if entry.status == "inconsistent" {
                    add_category(row, CATEGORY_DERIVED_LAG);
                    report.ok = false;
                    false
                } else if entry.status == "parse_failed" {
                    add_category(row, CATEGORY_PARSE_FAILED);
                    derived_sources_failed += 1;
                    report.ok = false;
                    false
                } else if entry.status == "unsupported" {
                    add_category(row, CATEGORY_UNSUPPORTED);
                    derived_sources_failed += 1;
                    report.ok = false;
                    false
                } else {
                    // A capture committed after the artifact was written means
                    // the derived view is behind even when record counts are
                    // unchanged (e.g. a whole-file recapture).
                    let stale_time = last_capture_ms
                        .map(|captured| captured > entry.replayed_at_ms)
                        .unwrap_or(false);
                    let stale_input = if entry.input_kind == "sqlite_observation" {
                        row.sqlite
                            .as_ref()
                            .map(|sqlite| entry.capture_seq < sqlite.observation_seq)
                            .unwrap_or(false)
                    } else {
                        entry.records < row.records_committed
                    };
                    stale_time || stale_input
                }
            }
        };
        if lagging {
            add_category(row, CATEGORY_DERIVED_LAG);
        } else if entry.is_some() {
            derived_sources_ok += 1;
        }
    }
    let mut derived_anomalies = derived_state_anomalies;
    derived_anomalies.extend(snapshot.anomalies.clone());
    if snapshot.status == "rebuilding" {
        // A rebuilding derived layer is not a normal state: derived reads fail
        // closed, so the coverage report must not claim health.
        report.ok = false;
        derived_anomalies.push(
            "derived layer is mid-rebuild; derived reads fail closed until replay completes"
                .to_string(),
        );
    }

    // 3. Scrub ledger: corruption is an anomaly for the affected sources. A
    //    corrupt ledger is itself reportable; it never turns into "no damage".
    match crate::scrub::read_ledger(archive) {
        Ok(Some(ledger)) => {
            let summary = ledger.summary();
            for entry in &ledger.corruption {
                for source in &entry.sources {
                    if let Some(row) = report.sources.iter_mut().find(|row| &row.source == source) {
                        row.corruption = true;
                        add_category(row, CATEGORY_CORRUPTION);
                        if row.category == CATEGORY_COVERED {
                            row.category = CATEGORY_CORRUPTION.to_string();
                        }
                    }
                }
                report.anomalies.push(format!(
                    "scrub: object {} is {} ({})",
                    entry.sha256, entry.status, entry.error
                ));
            }
            if !ledger.corruption.is_empty() {
                report.ok = false;
            }
            report.scrub = Some(summary);
        }
        Ok(None) => {}
        Err(error) => {
            report.ok = false;
            report.anomalies
                .push(format!("scrub ledger is unreadable: {error}"));
            report.scrub = Some(crate::scrub::ScrubCoverage {
                status: "ledger_corrupt".to_string(),
                checked_at_ms: 0,
                objects_checked: 0,
                bytes_checked: 0,
                corrupted: 0,
                missing: 0,
                orphan_objects: 0,
                staging_skipped: 0,
                anomalies: vec![error.to_string()],
            });
        }
    }

    // 4. Framework baseline reconciliation.
    let mut baselines = Vec::new();
    let mut reconciliation_anomalies = Vec::new();
    for declared in &config.sources {
        let Some(spec) = declared.baseline.as_ref() else {
            continue;
        };
        let result = evaluate_baseline(archive, declared, spec, report)?;
        if result.status == "mismatch" || result.status == "unavailable" {
            report.ok = false;
            for anomaly in &result.anomalies {
                reconciliation_anomalies.push(format!(
                    "baseline:{}:{}: {anomaly}",
                    result.kind, result.source_id
                ));
            }
        }
        baselines.push(result);
    }
    if config
        .sources
        .iter()
        .any(|declared| declared.enabled && format_generation_supported(declared).is_err())
    {
        report.ok = false;
    }
    report.reconciliation = ReconciliationReport {
        ok: baselines
            .iter()
            .all(|baseline| baseline.status != "mismatch" && baseline.status != "unavailable"),
        baselines,
        anomalies: reconciliation_anomalies.clone(),
    };
    report.anomalies.extend(reconciliation_anomalies);

    // 5. Category totals + primary category precedence.
    for row in report.sources.iter_mut() {
        let blocked = row.categories.iter().any(|category| {
            matches!(
                category.as_str(),
                CATEGORY_READ_FAILED
                    | CATEGORY_UNSUPPORTED
                    | CATEGORY_DISABLED
                    | CATEGORY_NOT_CAPTURED
                    | CATEGORY_CORRUPTION
            )
        });
        let has_evidence = row.records_committed > 0
            || row.bytes_total > 0
            || row
                .sqlite
                .as_ref()
                .map(|sqlite| sqlite.observation_seq > 0)
                .unwrap_or(false);
        if !blocked && has_evidence {
            // Raw evidence coverage is a separate dimension from attachment
            // completeness / derived freshness: a source can be `covered` and
            // `attachment_missing` or `derived_lag` at the same time.
            add_category(row, CATEGORY_COVERED);
        }
        if row.category.is_empty() {
            row.category = primary_category(&row.categories);
        }
    }
    let mut categories = CategoryTotals::default();
    for row in &report.sources {
        let mut counted = BTreeSet::new();
        for category in &row.categories {
            if counted.insert(category.clone()) {
                categories.count(category);
            }
        }
    }
    report.categories = categories;

    // 6. Provider aggregation.
    let mut providers: BTreeMap<String, ProviderReport> = BTreeMap::new();
    for provider in crate::SUPPORTED_PROVIDERS {
        providers.insert(
            provider.to_string(),
            ProviderReport {
                provider: provider.to_string(),
                ..ProviderReport::default()
            },
        );
    }
    for declared in &config.sources {
        let entry = providers
            .entry(declared.provider.clone())
            .or_insert_with(|| ProviderReport {
                provider: declared.provider.clone(),
                ..ProviderReport::default()
            });
        entry.declared_sources += 1;
        if declared.enabled {
            entry.enabled = true;
        }
    }
    for row in &report.sources {
        let entry = providers
            .entry(row.provider.clone())
            .or_insert_with(|| ProviderReport {
                provider: row.provider.clone(),
                ..ProviderReport::default()
            });
        entry.archived_sources += 1;
        entry.discovered += row.discovered_latest;
        entry.captured += row.captured_total;
        entry.skipped += row.skipped_total;
        for category in &row.categories {
            match category.as_str() {
                CATEGORY_COVERED => entry.covered_sources += 1,
                CATEGORY_DISABLED => entry.disabled_sources += 1,
                CATEGORY_UNSUPPORTED => entry.unsupported_sources += 1,
                CATEGORY_READ_FAILED => entry.read_failed_sources += 1,
                CATEGORY_PARSE_FAILED => entry.parse_failed_sources += 1,
                CATEGORY_ATTACHMENT_MISSING => entry.attachment_missing_sources += 1,
                CATEGORY_DERIVED_LAG => entry.derived_lagging_sources += 1,
                _ => {}
            }
        }
    }
    // Providers without declarations but with archived evidence are enabled by
    // evidence presence (and never silently marked disabled).
    for entry in providers.values_mut() {
        if entry.declared_sources == 0 && entry.archived_sources > 0 {
            entry.enabled = true;
        }
    }
    report.providers = providers.into_values().collect();

    // 7. Derived report.
    let mut sources_lagging = 0u64;
    for row in &report.sources {
        if row
            .categories
            .iter()
            .any(|value| value == CATEGORY_DERIVED_LAG)
        {
            sources_lagging += 1;
        }
    }
    report.derived = Some(DerivedReport {
        present: snapshot.generation.is_some(),
        generation: snapshot.generation,
        status: snapshot.status.clone(),
        parser_version: identity.parser_version.clone(),
        model_version: identity.model_version,
        sources_ok: derived_sources_ok,
        sources_lagging,
        sources_failed: derived_sources_failed,
        anomalies: derived_anomalies,
    });
    report.anomalies.extend(
        report
            .derived
            .as_ref()
            .map(|derived| derived.anomalies.clone())
            .unwrap_or_default(),
    );

    // Snapshot anomalies were already counted once; keep `ok` driven by
    // current-view failures, not by lifetime counters.
    Ok(())
}

fn primary_category(categories: &[String]) -> String {
    for candidate in [
        CATEGORY_UNSUPPORTED,
        CATEGORY_READ_FAILED,
        CATEGORY_PARSE_FAILED,
        CATEGORY_ATTACHMENT_MISSING,
        CATEGORY_CORRUPTION,
        CATEGORY_DISABLED,
        CATEGORY_DERIVED_LAG,
        CATEGORY_NOT_CAPTURED,
        CATEGORY_COVERED,
    ] {
        if categories.iter().any(|value| value == candidate) {
            return candidate.to_string();
        }
    }
    CATEGORY_COVERED.to_string()
}

fn format_generation_supported(declared: &DeclaredSource) -> Result<(), String> {
    if !crate::SUPPORTED_PROVIDERS.contains(&declared.provider.as_str()) {
        return Err(format!(
            "provider {:?} is not supported by this crate (supported: {:?})",
            declared.provider,
            crate::SUPPORTED_PROVIDERS
        ));
    }
    let Some(generation) = declared.format_generation.as_deref() else {
        return Ok(());
    };
    let matrix = crate::format_support_matrix();
    match matrix
        .iter()
        .find(|entry| entry.provider == declared.provider && entry.generation == generation)
    {
        Some(entry) if entry.supported => Ok(()),
        Some(entry) => Err(format!(
            "format generation {} for {} is explicitly unsupported: {}",
            entry.generation, entry.provider, entry.note
        )),
        None => Err(format!(
            "unknown format generation {generation:?} for {}; not in the P0 support matrix",
            declared.provider
        )),
    }
}

