//! Stage 7 §B: derived-text key-entity grounding, correction report and
//! old-vs-new statement conflicts (`agent-memory audit-grounding`).
//!
//! The command answers one question per *derived* text: are the mechanically
//! extractable key items of this text present in the original text it claims to
//! summarize? It **only reads**: the knowledge-base summaries, the learning
//! store and the live full-text indexes are never written, no archive object is
//! created and no model is called.
//!
//! # Input pairs
//!
//! * `--kb-summaries <vault-root>`: every `<vault-root>/.meta/summaries/**/*.md`
//!   body against the original file named by its frontmatter `source`
//!   (vault-relative; `..`/absolute/drive paths are refused). A non-text
//!   original is `skipped: binary_source`, a missing one `skipped:
//!   source_missing`.
//! * `--learning-root <R>` (+ explicit `--archive <abs> --domain <d>`): one
//!   knowledge candidate `claim` against the read-back text of all its
//!   `evidence[].ref`s, using the gate v1 reference shape and fingerprint check.
//!   Without an archive the whole class is `skipped: archive_not_supplied`; a
//!   ref that cannot be read is `skipped: ref_unreadable`. `applicability` is a
//!   generalization condition, not a restatement of evidence: its ungrounded
//!   items are counted and disclosed separately and never change the exit code.
//!
//! # Matching and the correction report
//!
//! Items and equivalence forms come from [`extract`]; the single normalizer is
//! the §A one. A match is a normalized substring. The read-only *correction
//! report* lists the most similar source items for every ungrounded item using
//! character 3-gram Jaccard similarity (the §A similarity function) — no model,
//! no rewriting of any derived text.
//!
//! # Body ratio
//!
//! Per input class the report totals derived and source bytes and counts single
//! documents whose derived text is longer than the original. It only discloses;
//! there is no threshold.
//!
//! # Conflicts
//!
//! `--conflicts --config <recall.json> [--since <rfc3339>]` adds the
//! old-vs-new statement pairs described in [`conflicts`]. `--out` writes the
//! whole report (grounding + conflicts) atomically and replaces the target.

pub mod conflicts;
pub mod extract;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime};

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use serde::Serialize;

use crate::archive::{hex_sha256, Archive};
use crate::learning::{gate, store, CandidateRecord, LearningStore, TASK_TARGET_KIND};

use extract::{extract_items, is_grounded, unit_was_dropped, KeyItem};

pub use conflicts::{
    ConflictPair, ConflictScan, ConflictSource, EXCLUSION_CONDITIONAL, EXCLUSION_EXAMPLE,
    EXCLUSION_NEGATED_QUESTION, EXCLUSION_QUESTION, EXCLUSION_QUOTATION, EXCLUSION_THIRD_PARTY,
    POLARITY_AFFIRMATIVE, POLARITY_NEGATIVE, STATUS_NEEDS_REVIEW,
};
pub use extract::{
    CorrectionCandidate, KeyItem as GroundingItem, KIND_DATE, KIND_IDENTIFIER, KIND_NUMBER,
    KIND_VERSION,
};

/// Report kind of a knowledge-base summary pair.
pub const PAIR_KB_SUMMARY: &str = "kb_summary";
/// Report kind of a learning knowledge-candidate pair.
pub const PAIR_LEARNING_CANDIDATE: &str = "learning_candidate";

/// `skipped` reason: the original is a binary document (`.docx`, `.pdf`, …).
pub const SKIP_BINARY_SOURCE: &str = "binary_source";
/// `skipped` reason: the original named by `source` does not exist.
pub const SKIP_SOURCE_MISSING: &str = "source_missing";
/// `skipped` reason: `source` escapes the vault root or is absolute.
pub const SKIP_PATH_ESCAPE: &str = "path_escape";
/// `skipped` reason: the summary frontmatter is missing `source`.
pub const SKIP_SUMMARY_INVALID: &str = "summary_invalid";
/// `skipped` reason: the original exists but is not UTF-8.
pub const SKIP_UNREADABLE_SOURCE: &str = "unreadable_source";
/// `skipped` reason: no `--archive --domain` was supplied for learning refs.
pub const SKIP_ARCHIVE_NOT_SUPPLIED: &str = "archive_not_supplied";
/// `skipped` reason: an evidence ref could not be read back / verified.
pub const SKIP_REF_UNREADABLE: &str = "ref_unreadable";

const SUMMARY_DIR: &str = ".meta/summaries";

/// Extensions treated as text originals. Everything else is `binary_source`.
const TEXT_EXTENSIONS: [&str; 16] = [
    "md", "markdown", "txt", "text", "rst", "adoc", "org", "csv", "tsv", "json", "jsonl", "yaml",
    "yml", "toml", "html", "htm",
];

/// One `audit-grounding` invocation.
#[derive(Debug, Clone, Default)]
pub struct GroundingOptions {
    pub kb_summaries: Option<PathBuf>,
    pub learning_root: Option<PathBuf>,
    /// Explicit absolute archive root; only meaningful with `--learning-root`.
    pub archive: Option<String>,
    /// Explicit access domain; required together with `archive`.
    pub domain: Option<String>,
    /// Scan live indexes for contradictory user statements.
    pub conflicts: bool,
    /// `recall.json`; required with `conflicts`.
    pub config: Option<PathBuf>,
    /// Only user units at or after this instant (conflicts only).
    pub since: Option<DateTime<FixedOffset>>,
    /// Upper bound of checked pairs (after sampling).
    pub limit: Option<usize>,
    /// Deterministic pair sample size.
    pub sample: Option<usize>,
    /// Sample seed; defaults to `0`.
    pub seed: Option<u64>,
}

/// A usage or runtime failure. `audit-grounding` has only exit 0, 1 and 2, so
/// every failure is a usage-class (2) error at the CLI boundary.
#[derive(Debug, Clone)]
pub struct GroundingError {
    pub code: &'static str,
    pub message: String,
}

impl GroundingError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::new("INVALID_USAGE", message)
    }

    fn io(path: &Path, error: std::io::Error) -> Self {
        Self::new(
            "GROUNDING_IO",
            format!("{}: {error}", path.display()),
        )
    }
}

impl std::fmt::Display for GroundingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for GroundingError {}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// One ungrounded derived item, with its position and correction candidates.
#[derive(Debug, Clone, Serialize)]
pub struct ItemFinding {
    pub kind: &'static str,
    pub text: String,
    pub offset: usize,
    pub line: usize,
    /// Short window of the derived text around the item.
    pub context: String,
    pub candidates: Vec<CorrectionCandidate>,
}

/// `applicability` grounding of one learning candidate (never affects exit).
#[derive(Debug, Clone, Serialize)]
pub struct ApplicabilityReport {
    pub items_checked: usize,
    pub items_ungrounded: usize,
    pub ungrounded: Vec<ItemFinding>,
}

/// One checked derived/source pair.
#[derive(Debug, Clone, Serialize)]
pub struct PairReport {
    pub kind: &'static str,
    pub id: String,
    /// Derived-text label (summary path or candidate id).
    pub derived: String,
    /// Source label (original path or evidence description).
    pub source: String,
    pub derived_bytes: usize,
    pub source_bytes: usize,
    pub items_checked: usize,
    pub items_ungrounded: usize,
    pub count_checked: usize,
    pub count_ungrounded: usize,
    /// Count-class items that did not occur in the source; never exit-affecting.
    pub count_items: Vec<ItemFinding>,
    pub unit_dropped: usize,
    pub ungrounded: Vec<ItemFinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applicability: Option<ApplicabilityReport>,
}

/// One row of the read-only correction report.
#[derive(Debug, Clone, Serialize)]
pub struct Correction {
    pub kind: &'static str,
    pub id: String,
    /// `claim` or `applicability`.
    pub list: &'static str,
    pub item_kind: &'static str,
    pub item: String,
    pub offset: usize,
    pub line: usize,
    pub candidates: Vec<CorrectionCandidate>,
}

/// Derived/source byte totals of one input class.
#[derive(Debug, Clone, Serialize)]
pub struct BodyRatio {
    pub kind: &'static str,
    pub pairs: usize,
    pub derived_bytes: usize,
    pub source_bytes: usize,
    pub ratio: f64,
    /// Pairs whose derived text is longer than the source text.
    pub single_over_source: usize,
}

/// What `--sample` selected.
#[derive(Debug, Clone, Serialize)]
pub struct SampleInfo {
    pub size: usize,
    pub seed: u64,
    pub pairs_total: usize,
    pub pairs_selected: usize,
}

/// The single JSON report of `audit-grounding`.
#[derive(Debug, Clone, Serialize)]
pub struct GroundingReport {
    pub ok: bool,
    pub command: &'static str,
    pub generated_at: String,
    pub elapsed_ms: u64,
    pub pairs_checked: usize,
    pub pairs_ungrounded: usize,
    pub items_checked: usize,
    pub items_ungrounded: usize,
    /// Small integers the summary itself counted (`5个问题`). Disclosed but
    /// never counted as ungrounded items.
    pub count_checked: usize,
    pub count_ungrounded: usize,
    /// Items grounded only through their numeric core because a `万`/`亿`
    /// magnitude spelling is absent from the source.
    pub unit_dropped: usize,
    /// `applicability` items checked (disclosed, never in the exit code).
    pub applicability_checked: usize,
    pub applicability_ungrounded: usize,
    pub skipped: BTreeMap<String, usize>,
    pub body_ratio: Vec<BodyRatio>,
    pub pairs: Vec<PairReport>,
    pub corrections: Vec<Correction>,
    pub conflicts: Vec<ConflictPair>,
    pub conflict_sources: Vec<ConflictSource>,
    pub conflict_excluded: BTreeMap<&'static str, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<SampleInfo>,
    pub notes: Vec<String>,
    /// §A measurement of this run (counters only; never derived text).
    pub cost: crate::cost::Cost,
}

impl GroundingReport {
    /// `0` when every checked item is grounded, `1` otherwise.
    pub fn exit_code(&self) -> u8 {
        if self.items_ungrounded > 0 {
            1
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Raw pairs
// ---------------------------------------------------------------------------

struct RawPair {
    kind: &'static str,
    id: String,
    derived_label: String,
    source_label: String,
    derived: String,
    source: String,
    derived_bytes: usize,
    source_bytes: usize,
    applicability: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the read-only check. Nothing is written except an explicit `--out`.
pub fn audit_grounding(options: &GroundingOptions) -> Result<GroundingReport, GroundingError> {
    let started = Instant::now();
    let start_io = crate::cost::sample_io().ok();
    validate_options(options)?;

    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut raw_pairs: Vec<RawPair> = Vec::new();
    if let Some(root) = options.kb_summaries.as_deref() {
        collect_kb_pairs(root, &mut raw_pairs, &mut skipped)?;
    }
    if let Some(root) = options.learning_root.as_deref() {
        collect_learning_pairs(root, options, &mut raw_pairs, &mut skipped)?;
    }

    let mut sample = None;
    if let Some(size) = options.sample {
        let seed = options.seed.unwrap_or(0);
        let total = raw_pairs.len();
        let selected = sample_pairs(raw_pairs, size, seed);
        sample = Some(SampleInfo {
            size,
            seed,
            pairs_total: total,
            pairs_selected: selected.len(),
        });
        raw_pairs = selected;
    }
    if let Some(limit) = options.limit {
        raw_pairs.truncate(limit);
    }

    let checked = check_pairs(&raw_pairs);

    let mut conflicts_report = ConflictScan::default();
    if options.conflicts {
        let path = options
            .config
            .as_deref()
            .expect("--config validated with --conflicts");
        let config = crate::recall::load_config(path)
            .map_err(|error| GroundingError::new("CONFIG_INVALID", error.to_string()))?;
        let since_ms = options.since.map(|stamp| stamp.timestamp_millis());
        conflicts_report = conflicts::scan_conflicts(&config, since_ms, options.limit);
        if conflicts_report
            .sources
            .iter()
            .any(|source| source.status != "scanned")
        {
            skipped.insert(
                "conflict_index_unavailable".to_string(),
                conflicts_report
                    .sources
                    .iter()
                    .filter(|source| source.status != "scanned")
                    .count(),
            );
        }
    }

    let checked_pairs = checked.pairs.len();
    let ok = checked.items_ungrounded == 0;
    let mut notes = Vec::new();
    if options.conflicts {
        notes.push(
            "conflicts read role=user text units of the live indexes only (assistant/tool and \
             agent compaction summaries are excluded); every pair is needs_review and nothing was \
             written"
                .to_string(),
        );
    }
    if checked.applicability_checked > 0 {
        notes.push(
            "applicability items are checked against the same evidence text and are disclosed \
             separately; they never affect the exit code"
                .to_string(),
        );
    }

    Ok(GroundingReport {
        ok,
        command: "audit-grounding",
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        elapsed_ms: started.elapsed().as_millis() as u64,
        pairs_checked: checked_pairs,
        pairs_ungrounded: checked.pairs_ungrounded,
        items_checked: checked.items_checked,
        items_ungrounded: checked.items_ungrounded,
        count_checked: checked.count_checked,
        count_ungrounded: checked.count_ungrounded,
        unit_dropped: checked.unit_dropped,
        applicability_checked: checked.applicability_checked,
        applicability_ungrounded: checked.applicability_ungrounded,
        skipped,
        body_ratio: checked.body_ratio,
        pairs: checked.pairs,
        corrections: checked.corrections,
        conflicts: conflicts_report.pairs,
        conflict_sources: conflicts_report.sources,
        conflict_excluded: conflicts_report.excluded,
        sample,
        notes,
        cost: crate::cost::Cost::sample(
            started.elapsed().as_millis() as u64,
            start_io,
            crate::cost::sample_io().ok(),
        ),
    })
}

/// Run the check and write the JSON report atomically to `path`.
pub fn audit_grounding_to_file(
    options: &GroundingOptions,
    path: &Path,
) -> Result<GroundingReport, GroundingError> {
    let report = audit_grounding(options)?;
    let mut bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| GroundingError::new("GROUNDING_SERIALIZE", error.to_string()))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)?;
    Ok(report)
}

fn validate_options(options: &GroundingOptions) -> Result<(), GroundingError> {
    if options.kb_summaries.is_none() && options.learning_root.is_none() && !options.conflicts {
        return Err(GroundingError::usage(
            "audit-grounding requires at least one of --kb-summaries, --learning-root, --conflicts",
        ));
    }
    if options.conflicts && options.config.is_none() {
        return Err(GroundingError::usage(
            "--conflicts requires --config <recall.json>",
        ));
    }
    if !options.conflicts && options.config.is_some() {
        return Err(GroundingError::usage(
            "--config is only used with --conflicts",
        ));
    }
    if options.since.is_some() && !options.conflicts {
        return Err(GroundingError::usage(
            "--since is only used with --conflicts",
        ));
    }
    match (&options.archive, &options.domain) {
        (None, None) => {}
        (Some(_), Some(domain)) if !domain.trim().is_empty() => {}
        (Some(_), Some(_)) => {
            return Err(GroundingError::usage(
                "--domain must be a non-empty access domain",
            ))
        }
        _ => {
            return Err(GroundingError::usage(
                "--archive and --domain must be given together",
            ))
        }
    }
    if options.archive.is_some() && options.learning_root.is_none() {
        return Err(GroundingError::usage(
            "--archive and --domain only apply with --learning-root",
        ));
    }
    if options.limit == Some(0) {
        return Err(GroundingError::usage("--limit must be at least 1"));
    }
    if options.sample == Some(0) {
        return Err(GroundingError::usage("--sample must be at least 1"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Knowledge-base summaries
// ---------------------------------------------------------------------------

fn collect_kb_pairs(
    vault: &Path,
    out: &mut Vec<RawPair>,
    skipped: &mut BTreeMap<String, usize>,
) -> Result<(), GroundingError> {
    if !vault.is_dir() {
        return Err(GroundingError::usage(format!(
            "--kb-summaries must name an existing vault root, got {}",
            vault.display()
        )));
    }
    let summaries_root = vault.join(SUMMARY_DIR);
    if !summaries_root.is_dir() {
        return Err(GroundingError::usage(format!(
            "{} does not exist under the vault root; nothing to check",
            summaries_root.display()
        )));
    }
    let mut files = Vec::new();
    walk_markdown(&summaries_root, &mut files)?;
    files.sort();

    for file in files {
        let relative = file
            .strip_prefix(vault)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(_) => {
                *skipped.entry(SKIP_SUMMARY_INVALID.to_string()).or_insert(0) += 1;
                continue;
            }
        };
        let Some((frontmatter, body)) = split_summary(&text) else {
            *skipped.entry(SKIP_SUMMARY_INVALID.to_string()).or_insert(0) += 1;
            continue;
        };
        let Some(source) = frontmatter.get("source").filter(|value| !value.trim().is_empty())
        else {
            *skipped.entry(SKIP_SUMMARY_INVALID.to_string()).or_insert(0) += 1;
            continue;
        };
        let Some(relative_source) = safe_relative(source) else {
            *skipped.entry(SKIP_PATH_ESCAPE.to_string()).or_insert(0) += 1;
            continue;
        };
        let extension = relative_source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !TEXT_EXTENSIONS.contains(&extension.as_str()) {
            *skipped.entry(SKIP_BINARY_SOURCE.to_string()).or_insert(0) += 1;
            continue;
        }
        let original_path = vault.join(&relative_source);
        if !original_path.is_file() {
            *skipped.entry(SKIP_SOURCE_MISSING.to_string()).or_insert(0) += 1;
            continue;
        }
        let original = match fs::read(&original_path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    *skipped.entry(SKIP_UNREADABLE_SOURCE.to_string()).or_insert(0) += 1;
                    continue;
                }
            },
            Err(error) => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    *skipped.entry(SKIP_SOURCE_MISSING.to_string()).or_insert(0) += 1;
                } else {
                    *skipped.entry(SKIP_UNREADABLE_SOURCE.to_string()).or_insert(0) += 1;
                }
                continue;
            }
        };
        out.push(RawPair {
            kind: PAIR_KB_SUMMARY,
            id: relative.clone(),
            derived_label: relative,
            source_label: source.clone(),
            derived_bytes: body.len(),
            source_bytes: original.len(),
            derived: body,
            source: original,
            applicability: Vec::new(),
        });
    }
    Ok(())
}

fn walk_markdown(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), GroundingError> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|error| GroundingError::io(dir, error))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk_markdown(&path, out)?;
        } else if path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Split a summary file into frontmatter fields and body text.
fn split_summary(text: &str) -> Option<(BTreeMap<String, String>, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut fields = BTreeMap::new();
    loop {
        let line = lines.next()?;
        if line.trim() == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches('"').trim_matches('\'').to_string();
            fields.insert(key.trim().to_ascii_lowercase(), value);
        }
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    Some((fields, body))
}

/// A vault-relative path that cannot escape the vault root.
fn safe_relative(source: &str) -> Option<PathBuf> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if relative.as_os_str().is_empty() {
        None
    } else {
        Some(relative)
    }
}

// ---------------------------------------------------------------------------
// Learning candidates
// ---------------------------------------------------------------------------

fn collect_learning_pairs(
    root: &Path,
    options: &GroundingOptions,
    out: &mut Vec<RawPair>,
    skipped: &mut BTreeMap<String, usize>,
) -> Result<(), GroundingError> {
    let store = LearningStore::open_read_only(root).map_err(|error| {
        GroundingError::new(
            "LEARNING_STORE_UNREADABLE",
            format!("learning root {} is not readable: {error}", root.display()),
        )
    })?;
    let mut statement = store
        .connection()
        .prepare("SELECT record_json FROM candidates ORDER BY candidate_id")
        .map_err(|error| GroundingError::new("LEARNING_STORE_UNREADABLE", error.to_string()))?;
    let records = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| GroundingError::new("LEARNING_STORE_UNREADABLE", error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| GroundingError::new("LEARNING_STORE_UNREADABLE", error.to_string()))?;
    drop(statement);

    let mut candidates: Vec<CandidateRecord> = Vec::new();
    for json in records {
        let record = store::parse_record(&json).map_err(|error| {
            GroundingError::new(
                "LEARNING_STORE_UNREADABLE",
                format!("stored candidate record is not readable: {error}"),
            )
        })?;
        if record.target_kind != TASK_TARGET_KIND {
            candidates.push(record);
        }
    }

    let Some(archive_root) = options.archive.as_deref() else {
        // The whole class is undisclosed-by-skip, not a failure.
        *skipped.entry(SKIP_ARCHIVE_NOT_SUPPLIED.to_string()).or_insert(0) += candidates.len();
        return Ok(());
    };
    let domain = options.domain.as_deref().unwrap_or_default();
    let archive = gate::open_archive(archive_root).map_err(|error| {
        GroundingError::new("ARCHIVE_UNREADABLE", format!("{error}"))
    })?;
    let index = archive.load_records().map_err(|error| {
        GroundingError::new(
            "ARCHIVE_UNREADABLE",
            format!("committed archive state is unreadable: {error}"),
        )
    })?;

    for record in candidates {
        let mut texts = Vec::new();
        let mut unreadable = 0usize;
        for item in &record.evidence {
            match read_reference_text(&archive, &index.records, domain, &item.reference) {
                Some(text) => texts.push(text),
                None => unreadable += 1,
            }
        }
        if texts.is_empty() {
            // Every ref (or the absence of refs) leaves nothing to ground.
            *skipped.entry(SKIP_REF_UNREADABLE.to_string()).or_insert(0) += 1;
            continue;
        }
        if unreadable > 0 {
            *skipped.entry(SKIP_REF_UNREADABLE.to_string()).or_insert(0) += unreadable;
        }
        let source = texts.join("\n");
        out.push(RawPair {
            kind: PAIR_LEARNING_CANDIDATE,
            id: record.candidate_id.clone(),
            derived_label: format!(
                "{} (revision {}, claim)",
                record.candidate_id, record.revision
            ),
            source_label: format!("{} archive evidence record(s)", texts.len()),
            derived_bytes: record.claim.len(),
            source_bytes: source.len(),
            derived: record.claim.clone(),
            source,
            applicability: record.applicability.clone(),
        });
    }
    Ok(())
}

fn read_reference_text(
    archive: &Archive,
    records: &BTreeMap<String, crate::archive::RecordRef>,
    domain: &str,
    reference: &str,
) -> Option<String> {
    let sha = gate::parse_reference(reference)?;
    if !records.contains_key(&sha) {
        return None;
    }
    let bytes = archive.reader(domain).read_record(&sha).ok()?;
    if hex_sha256(&bytes) != sha {
        return None;
    }
    String::from_utf8(bytes).ok()
}

// ---------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------

struct Checked {
    pairs: Vec<PairReport>,
    pairs_ungrounded: usize,
    items_checked: usize,
    items_ungrounded: usize,
    count_checked: usize,
    count_ungrounded: usize,
    unit_dropped: usize,
    applicability_checked: usize,
    applicability_ungrounded: usize,
    corrections: Vec<Correction>,
    body_ratio: Vec<BodyRatio>,
}

fn check_pairs(raw_pairs: &[RawPair]) -> Checked {
    let mut pairs = Vec::with_capacity(raw_pairs.len());
    let mut corrections = Vec::new();
    let mut pairs_ungrounded = 0usize;
    let mut items_checked = 0usize;
    let mut items_ungrounded = 0usize;
    let mut count_checked = 0usize;
    let mut count_ungrounded = 0usize;
    let mut unit_dropped = 0usize;
    let mut applicability_checked = 0usize;
    let mut applicability_ungrounded = 0usize;
    let mut ratios: BTreeMap<&'static str, (usize, usize, usize, usize)> = BTreeMap::new();

    for raw in raw_pairs {
        let normalized_source = extract::normalize(&raw.source);
        let mut source_items: Option<Vec<KeyItem>> = None;
        let items = extract_items(&raw.derived);
        let mut ungrounded = Vec::new();
        let mut count_items = Vec::new();
        let mut pair_count_checked = 0usize;
        let mut pair_unit_dropped = 0usize;
        for item in &items {
            if item.kind == extract::KIND_COUNT {
                // The summary's own count ("指出了5个问题"): disclosed, never
                // exit-affecting.
                pair_count_checked += 1;
                count_checked += 1;
                if !is_grounded(&item.forms, &normalized_source) {
                    count_ungrounded += 1;
                    count_items.push(ItemFinding {
                        kind: item.kind,
                        text: item.text.clone(),
                        offset: item.offset,
                        line: item.line,
                        context: context_window(&raw.derived, item),
                        candidates: Vec::new(),
                    });
                }
                continue;
            }
            items_checked += 1;
            if is_grounded(&item.forms, &normalized_source) {
                if unit_was_dropped(item, &normalized_source) {
                    pair_unit_dropped += 1;
                    unit_dropped += 1;
                }
                continue;
            }
            items_ungrounded += 1;
            let candidates = source_items.get_or_insert_with(|| extract_items(&raw.source));
            let nearest = extract::corrections(item, candidates, extract::CORRECTION_CANDIDATES);
            corrections.push(Correction {
                kind: raw.kind,
                id: raw.id.clone(),
                list: "claim",
                item_kind: item.kind,
                item: item.text.clone(),
                offset: item.offset,
                line: item.line,
                candidates: nearest.clone(),
            });
            ungrounded.push(ItemFinding {
                kind: item.kind,
                text: item.text.clone(),
                offset: item.offset,
                line: item.line,
                context: context_window(&raw.derived, item),
                candidates: nearest,
            });
        }

        let applicability = if raw.applicability.is_empty() {
            None
        } else {
            let mut report = ApplicabilityReport {
                items_checked: 0,
                items_ungrounded: 0,
                ungrounded: Vec::new(),
            };
            for condition in &raw.applicability {
                for item in extract_items(condition) {
                    report.items_checked += 1;
                    applicability_checked += 1;
                    if is_grounded(&item.forms, &normalized_source) {
                        continue;
                    }
                    report.items_ungrounded += 1;
                    applicability_ungrounded += 1;
                    let candidates =
                        source_items.get_or_insert_with(|| extract_items(&raw.source));
                    let nearest =
                        extract::corrections(&item, candidates, extract::CORRECTION_CANDIDATES);
                    corrections.push(Correction {
                        kind: raw.kind,
                        id: raw.id.clone(),
                        list: "applicability",
                        item_kind: item.kind,
                        item: item.text.clone(),
                        offset: item.offset,
                        line: item.line,
                        candidates: nearest.clone(),
                    });
                    report.ungrounded.push(ItemFinding {
                        kind: item.kind,
                        text: item.text.clone(),
                        offset: item.offset,
                        line: item.line,
                        context: context_window(condition, &item),
                        candidates: nearest,
                    });
                }
            }
            Some(report)
        };

        if !ungrounded.is_empty() {
            pairs_ungrounded += 1;
        }
        let entry = ratios
            .entry(raw.kind)
            .or_insert((0usize, 0usize, 0usize, 0usize));
        entry.0 += 1;
        entry.1 += raw.derived_bytes;
        entry.2 += raw.source_bytes;
        if raw.derived_bytes > raw.source_bytes {
            entry.3 += 1;
        }
        pairs.push(PairReport {
            kind: raw.kind,
            id: raw.id.clone(),
            derived: raw.derived_label.clone(),
            source: raw.source_label.clone(),
            derived_bytes: raw.derived_bytes,
            source_bytes: raw.source_bytes,
            items_checked: items.iter().filter(|item| item.kind != extract::KIND_COUNT).count(),
            items_ungrounded: ungrounded.len(),
            count_checked: pair_count_checked,
            count_ungrounded: count_items.len(),
            count_items,
            unit_dropped: pair_unit_dropped,
            ungrounded,
            applicability,
        });
    }

    let body_ratio = ratios
        .into_iter()
        .map(|(kind, (pairs, derived, source, over))| BodyRatio {
            kind,
            pairs,
            derived_bytes: derived,
            source_bytes: source,
            ratio: if source == 0 {
                0.0
            } else {
                derived as f64 / source as f64
            },
            single_over_source: over,
        })
        .collect();

    Checked {
        pairs,
        pairs_ungrounded,
        items_checked,
        items_ungrounded,
        count_checked,
        count_ungrounded,
        unit_dropped,
        applicability_checked,
        applicability_ungrounded,
        corrections,
        body_ratio,
    }
}

fn context_window(text: &str, item: &KeyItem) -> String {
    crate::recall::text::snippet(text, item.offset, item.text.len(), 120).0
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// Deterministic sample: the same seed and the same pair set always select the
/// same pairs, in their original order.
fn sample_pairs(pairs: Vec<RawPair>, size: usize, seed: u64) -> Vec<RawPair> {
    if size >= pairs.len() || pairs.is_empty() {
        return pairs;
    }
    let mut indices: Vec<usize> = (0..pairs.len()).collect();
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    for position in 0..size {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let span = indices.len() - position;
        let pick = position + (state as usize % span);
        indices.swap(position, pick);
    }
    indices.truncate(size);
    indices.sort_unstable();

    let mut slots: Vec<Option<RawPair>> = pairs.into_iter().map(Some).collect();
    let mut selected = Vec::with_capacity(size);
    for index in indices {
        if let Some(pair) = slots[index].take() {
            selected.push(pair);
        }
    }
    selected
}

// ---------------------------------------------------------------------------
// Atomic output
// ---------------------------------------------------------------------------

/// Write `bytes` to `path` through a sibling temporary file and one rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), GroundingError> {
    let parent = path.parent().ok_or_else(|| {
        GroundingError::usage(format!(
            "--out must name a file inside an existing directory, got {}",
            path.display()
        ))
    })?;
    if !parent.is_dir() {
        return Err(GroundingError::usage(format!(
            "--out directory {} does not exist",
            parent.display()
        )));
    }
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "grounding-last.json".to_string());
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(".{file_name}.tmp-{}-{stamp}", std::process::id()));
    if let Err(error) = fs::write(&temporary, bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(GroundingError::io(&temporary, error));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(GroundingError::io(path, error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(id: &str, derived: &str, source: &str, applicability: Vec<String>) -> RawPair {
        RawPair {
            kind: PAIR_KB_SUMMARY,
            id: id.to_string(),
            derived_label: id.to_string(),
            source_label: "src".to_string(),
            derived: derived.to_string(),
            source: source.to_string(),
            derived_bytes: derived.len(),
            source_bytes: source.len(),
            applicability,
        }
    }

    #[test]
    fn safe_relative_refuses_escapes_and_absolute_paths() {
        assert_eq!(
            safe_relative("notes/2026.md"),
            Some(PathBuf::from("notes").join("2026.md"))
        );
        for source in ["../x.md", "a/../../x.md", "C:/x.md", "/etc/passwd", ""] {
            assert!(safe_relative(source).is_none(), "{source:?} must be refused");
        }
    }

    #[test]
    fn summary_frontmatter_is_split_from_the_body() {
        let text = "---\nsource: a/b.md\ncontent_hash: 0123456789abcdef\n---\n摘要 2026-11-07\n";
        let (fields, body) = split_summary(text).expect("valid frontmatter");
        assert_eq!(fields["source"], "a/b.md");
        assert_eq!(body, "摘要 2026-11-07");
        assert!(split_summary("no frontmatter").is_none());
    }

    #[test]
    fn checking_counts_pairs_items_and_applicability_separately() {
        let checked = check_pairs(&[
            pair("grounded", "2026-11-07 共 1129 篇", "2026年11月7日 1,129 篇", vec![]),
            pair("ungrounded", "2026-11-08 共 1130 篇，另附一份很长的说明文字用于超过原文", "2026年11月7日 1,129 篇", vec![]),
            pair(
                "applicability",
                "2026-11-07",
                "2026年11月7日",
                vec!["只在 Windows 2026-11-08 上".to_string()],
            ),
            pair("count", "指出了5个问题", "问题列表，没有任何数字", vec![]),
        ]);
        assert_eq!(checked.pairs.len(), 4);
        assert_eq!(checked.pairs_ungrounded, 1);
        assert_eq!(checked.items_ungrounded, 2);
        assert_eq!(checked.count_checked, 1);
        assert_eq!(checked.count_ungrounded, 1);
        // A count never enters the exit-affecting item total.
        assert_eq!(checked.items_checked, 5);
        assert_eq!(checked.pairs[3].count_ungrounded, 1);
        assert_eq!(checked.pairs[3].count_items.len(), 1);
        assert_eq!(checked.pairs[3].items_ungrounded, 0);
        assert_eq!(checked.applicability_checked, 1);
        assert_eq!(checked.applicability_ungrounded, 1);
        assert_eq!(checked.body_ratio.len(), 1);
        assert_eq!(checked.body_ratio[0].single_over_source, 1);
        assert_eq!(checked.corrections.len(), 3);
        let dates: Vec<&str> = checked
            .corrections
            .iter()
            .flat_map(|correction| correction.candidates.iter())
            .map(|candidate| candidate.text.as_str())
            .collect();
        assert!(dates.contains(&"2026年11月7日"), "{dates:?}");
    }

    #[test]
    fn sampling_is_deterministic_for_a_seed() {
        let make = || {
            (0..20)
                .map(|index| pair(&format!("p{index}"), "x", "y", vec![]))
                .collect::<Vec<_>>()
        };
        let first: Vec<String> = sample_pairs(make(), 5, 7)
            .into_iter()
            .map(|pair| pair.id)
            .collect();
        let second: Vec<String> = sample_pairs(make(), 5, 7)
            .into_iter()
            .map(|pair| pair.id)
            .collect();
        let other: Vec<String> = sample_pairs(make(), 5, 8)
            .into_iter()
            .map(|pair| pair.id)
            .collect();
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(first.len(), 5);
    }

    #[test]
    fn atomic_write_replaces_the_target() {
        let directory = tempfile::tempdir().expect("temp");
        let path = directory.path().join("grounding-last.json");
        std::fs::write(&path, b"old").expect("seed");
        write_atomic(&path, b"new").expect("atomic write");
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
        let leftovers = std::fs::read_dir(directory.path())
            .expect("dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
    }
}
