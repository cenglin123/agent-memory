//! `eval-recall`: the unified retrieval cross-check (stage 7 §C).
//!
//! Every question is answered by *the same* implementation `memory_search`
//! uses (`SearchRequest::parse` + `memory_search_with`); this module only adds
//! measurement, source-group control, the frozen KB time decay and the corpus
//! fingerprint around the run. Nothing here writes an index, the learning
//! store or the knowledge base: the single possible write is the explicit
//! `--out` report (atomic, overwrite-style).
//!
//! Report shape (stable, `schema_version = 1`):
//!
//! ```json
//! {"ok":true,"command":"eval-recall","schema_version":1,"generated_at":"…",
//!  "queries":12,"repeat":1,"groups":[…],"corpus":{…},"comparison":{…},
//!  "rewrites":[…],"against":{…},"nondeterministic":0}
//! ```
//!
//! A report carries question ids, metrics, ranks and refs only. Retrieved text
//! never enters it (the question set itself may contain real content, the
//! report does not copy it).

use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::status;
use crate::recall::fulltext::coverage;
use crate::recall::search::{memory_search_with, SearchOptions, SearchRequest, SearchResult, SearchResponse};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

/// Report schema. Bumping it is a corpus-comparison contract change.
pub const EVAL_SCHEMA_VERSION: u32 = 1;
pub const EVAL_COMMAND: &str = "eval-recall";
/// The three question kinds (§C).
pub const EVAL_QUERY_KINDS: [&str; 3] = [
    "single_session_quote",
    "cross_session_fact",
    "kb_multi_doc",
];
/// The three source groups, in report order.
pub const EVAL_GROUPS: [&str; 3] = ["sessions", "kb", "all"];
/// `hit@k` needs the first ten results; the request asks for exactly that.
pub const EVAL_LIMIT: usize = 10;
/// Snippet ceiling: gold snippets are matched against the returned snippet, so
/// the eval asks for the largest allowed window.
pub const EVAL_SNIPPET_BYTES: usize = 8_192;
/// `--repeat` upper bound: a typo must not turn into an unbounded run.
pub const EVAL_REPEAT_MAX: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Sessions,
    Kb,
    All,
}

impl Group {
    pub fn as_str(&self) -> &'static str {
        match self {
            Group::Sessions => "sessions",
            Group::Kb => "kb",
            Group::All => "all",
        }
    }

    pub fn parse(raw: &str) -> Option<Group> {
        match raw {
            "sessions" => Some(Group::Sessions),
            "kb" => Some(Group::Kb),
            "all" => Some(Group::All),
            _ => None,
        }
    }

    fn accepts(&self, kind: SourceKind) -> bool {
        match self {
            // Session records are the evidence layer: the six session kinds.
            // `device` notes and `kb` are not session records.
            Group::Sessions => matches!(
                kind,
                SourceKind::Opencode
                    | SourceKind::Codex
                    | SourceKind::ClaudeCode
                    | SourceKind::Dsh
                    | SourceKind::Hermes
                    | SourceKind::Archive
            ),
            Group::Kb => kind == SourceKind::Kb,
            Group::All => true,
        }
    }
}

/// One gold item: the result must come from `source_id` and carry the same
/// session id and/or the same `rel_path`; an optional `snippet` must occur in
/// the returned snippet after normalization.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Gold {
    pub source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rel_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// One validated question.
#[derive(Debug, Clone)]
pub struct EvalQuery {
    pub line: usize,
    pub id: String,
    pub query: String,
    pub mode: Option<String>,
    pub sources: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub kind: String,
    pub gold: Vec<Gold>,
    pub negative: bool,
    pub rewrite_of: Option<String>,
}

/// One rejected line of the question set.
#[derive(Debug, Clone, Serialize)]
pub struct QueryIssue {
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub message: String,
}

#[derive(Debug)]
pub enum EvalError {
    /// The question set is invalid: every bad line is reported, nothing ran.
    Queries(Vec<QueryIssue>),
    /// A usage or infrastructure failure (config, file, serialization).
    Runtime { code: &'static str, message: String },
}

impl EvalError {
    fn runtime(message: impl Into<String>) -> EvalError {
        EvalError::Runtime {
            code: "EVAL_RECALL_FAILED",
            message: message.into(),
        }
    }

    fn coded(code: &'static str, message: impl Into<String>) -> EvalError {
        EvalError::Runtime {
            code,
            message: message.into(),
        }
    }
}

const QUERY_KEYS: [&str; 9] = [
    "id",
    "query",
    "mode",
    "sources",
    "session_id",
    "kind",
    "gold",
    "negative",
    "rewrite_of",
];
const GOLD_KEYS: [&str; 4] = ["source_id", "rel_path", "session_id", "snippet"];

/// Parse and validate a JSONL question set. Every bad line is collected; an
/// invalid set is rejected as a whole (the caller never runs half of it).
pub fn parse_queries(text: &str) -> Result<Vec<EvalQuery>, EvalError> {
    // PowerShell 5.1 writes UTF-8 with BOM by default; exactly one leading BOM
    // is removed before the first line is parsed (same rule as CLI `@file`).
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut queries: Vec<EvalQuery> = Vec::new();
    let mut issues: Vec<QueryIssue> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                issues.push(QueryIssue {
                    line,
                    id: None,
                    message: format!("line is not valid JSON: {error}"),
                });
                continue;
            }
        };
        match parse_query(&value, line) {
            Ok(query) => queries.push(query),
            Err(message) => issues.push(QueryIssue {
                line,
                id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                message,
            }),
        }
    }

    // Cross-line rules: unique ids, and `rewrite_of` must name another line.
    let ids: Vec<(usize, String)> = queries.iter().map(|q| (q.line, q.id.clone())).collect();
    let mut duplicates: Vec<(usize, String)> = Vec::new();
    for (index, (line, id)) in ids.iter().enumerate() {
        if ids[..index].iter().any(|(_, existing)| existing == id) {
            duplicates.push((*line, id.clone()));
        }
    }
    for (line, id) in duplicates {
        issues.push(QueryIssue {
            line,
            id: Some(id.clone()),
            message: format!("duplicate question id {id:?}"),
        });
    }
    for query in &queries {
        let Some(target) = query.rewrite_of.as_deref() else {
            continue;
        };
        if target == query.id {
            issues.push(QueryIssue {
                line: query.line,
                id: Some(query.id.clone()),
                message: "rewrite_of must not name the question itself".to_string(),
            });
        } else if !ids.iter().any(|(_, id)| id == target) {
            issues.push(QueryIssue {
                line: query.line,
                id: Some(query.id.clone()),
                message: format!("rewrite_of names the unknown question {target:?}"),
            });
        }
    }

    if !issues.is_empty() {
        issues.sort_by_key(|issue| issue.line);
        return Err(EvalError::Queries(issues));
    }
    if queries.is_empty() {
        return Err(EvalError::Queries(vec![QueryIssue {
            line: 0,
            id: None,
            message: "the question set is empty (no JSONL line)".to_string(),
        }]));
    }
    Ok(queries)
}

fn parse_query(value: &Value, line: usize) -> Result<EvalQuery, String> {
    let map = value.as_object().ok_or("line must be a JSON object")?;
    if let Some(key) = map.keys().find(|key| !QUERY_KEYS.contains(&key.as_str())) {
        return Err(format!("unknown field {key:?}"));
    }

    let id = string_field(map, "id")?;
    let query_text = string_field(map, "query")?;
    let kind = string_field(map, "kind")?;
    if !EVAL_QUERY_KINDS.contains(&kind.as_str()) {
        return Err(format!(
            "kind must be one of {}; got {kind:?}",
            EVAL_QUERY_KINDS.join(", ")
        ));
    }
    let mode = optional_string(map, "mode")?;
    if let Some(mode) = &mode {
        if !matches!(mode.as_str(), "auto" | "exact" | "hybrid" | "legacy") {
            return Err(format!(
                "mode must be auto, exact, hybrid or legacy; got {mode:?}"
            ));
        }
    }
    let session_id = optional_string(map, "session_id")?;
    let sources = match map.get("sources") {
        None => None,
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err("sources must not be an empty array".to_string());
            }
            let mut out: Vec<String> = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let Some(id) = item.as_str() else {
                    return Err(format!("sources[{index}] must be a string"));
                };
                if id.is_empty() {
                    return Err(format!("sources[{index}] must not be empty"));
                }
                if out.iter().any(|existing| existing == id) {
                    return Err(format!("sources[{index}] repeats {id:?}"));
                }
                out.push(id.to_string());
            }
            Some(out)
        }
        Some(_) => return Err("sources must be an array of source ids".to_string()),
    };
    let negative = match map.get("negative") {
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("negative must be a boolean".to_string()),
        None => return Err("missing required field \"negative\"".to_string()),
    };
    let gold = match map.get("gold") {
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                out.push(parse_gold(item, index)?);
            }
            out
        }
        Some(_) => return Err("gold must be an array".to_string()),
        None => return Err("missing required field \"gold\"".to_string()),
    };
    if negative {
        if !gold.is_empty() {
            return Err("a negative question must not carry gold items".to_string());
        }
    } else if gold.is_empty() {
        return Err("a positive question needs at least one gold item".to_string());
    }
    let rewrite_of = optional_string(map, "rewrite_of")?;

    Ok(EvalQuery {
        line,
        id,
        query: query_text,
        mode,
        sources,
        session_id,
        kind,
        gold,
        negative,
        rewrite_of,
    })
}

fn parse_gold(value: &Value, index: usize) -> Result<Gold, String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("gold[{index}] must be an object"))?;
    if let Some(key) = map.keys().find(|key| !GOLD_KEYS.contains(&key.as_str())) {
        return Err(format!("gold[{index}] has unknown field {key:?}"));
    }
    let source_id = map
        .get("source_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("gold[{index}].source_id is required"))?;
    let rel_path = optional_string(map, "rel_path")?;
    let session_id = optional_string(map, "session_id")?;
    let snippet = optional_string(map, "snippet")?;
    if rel_path.is_none() && session_id.is_none() {
        return Err(format!(
            "gold[{index}] needs a rel_path and/or a session_id (source_id alone is not an identity)"
        ));
    }
    Ok(Gold {
        source_id: source_id.to_string(),
        rel_path,
        session_id,
        snippet,
    })
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    match map.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(Value::String(_)) => Err(format!("{key} must not be empty")),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("missing required field {key:?}")),
    }
}

fn optional_string(map: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(Value::String(_)) => Err(format!("{key} must not be empty")),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Metrics {
    pub queries: usize,
    pub positives: usize,
    pub negatives: usize,
    pub hit_at_1: f64,
    pub hit_at_5: f64,
    pub hit_at_10: f64,
    pub mrr: f64,
    pub negative_empty_rate: f64,
    pub source_status_entries: usize,
    pub source_status_non_ok: usize,
    pub source_status_non_ok_ratio: f64,
    pub errors: usize,
    pub latency_ms_p50: u64,
    pub latency_ms_p95: u64,
    pub unavailable: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalRef {
    pub rank: usize,
    pub source_id: String,
    pub source_kind: String,
    pub rel_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResult {
    pub id: String,
    pub kind: String,
    pub negative: bool,
    /// `ran` / `unavailable` (the question named no source in this group).
    pub status: String,
    /// For a positive question: some gold matched inside the returned top-10.
    /// For a negative question: the result list was empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit: Option<bool>,
    /// 1-based rank of the first matching result (positive questions only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<usize>,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub source_status_entries: usize,
    #[serde(default)]
    pub source_status_non_ok: usize,
    #[serde(default)]
    pub errors: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top: Vec<EvalRef>,
    #[serde(default)]
    pub nondeterministic: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub differences: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupReport {
    pub group: String,
    /// `ran` or `skipped_no_sources`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    #[serde(default)]
    pub by_kind: BTreeMap<String, Metrics>,
    #[serde(default)]
    pub queries: Vec<QueryResult>,
    #[serde(default)]
    pub nondeterministic: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Delta {
    pub hit_at_1: f64,
    pub hit_at_5: f64,
    pub hit_at_10: f64,
    pub mrr: f64,
    pub negative_empty_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupRow {
    pub group: String,
    pub positives: usize,
    pub negatives: usize,
    pub hit_at_1: f64,
    pub hit_at_5: f64,
    pub hit_at_10: f64,
    pub mrr: f64,
    pub negative_empty_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_vs_baseline: Option<Delta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KindRow {
    pub kind: String,
    pub rows: Vec<GroupRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComparisonReport {
    pub baseline: String,
    pub rows: Vec<GroupRow>,
    #[serde(default)]
    pub by_kind: Vec<KindRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RewriteRow {
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub natural_hit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub natural_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier_hit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier_rank: Option<usize>,
    /// `identifier_hit - natural_hit` (`None` when either side did not run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_delta: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RewriteReport {
    pub natural: String,
    pub identifier: String,
    pub natural_mode: String,
    pub identifier_mode: String,
    pub kind: String,
    pub rows: Vec<RewriteRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorpusSource {
    pub source_id: String,
    pub kind: String,
    /// `tracked`, `untracked` (kb) or `unavailable` (no readable index).
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default)]
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CorpusReport {
    pub tracked: usize,
    pub untracked: usize,
    pub unavailable: usize,
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub sources: Vec<CorpusSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgainstChange {
    pub id: String,
    pub kind: String,
    pub group: String,
    /// `new_hit`, `new_miss`, `rank_changed`.
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_hit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_hit: Option<bool>,
    /// `before_rank - after_rank`: positive means the hit moved up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank_delta: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgainstReport {
    pub path: String,
    pub generated_at: String,
    pub group: String,
    pub new_hits: usize,
    pub new_misses: usize,
    pub rank_changes: usize,
    pub unchanged: usize,
    pub missing: usize,
    /// Questions in this run that the prior report does not have.
    #[serde(default)]
    pub added: usize,
    #[serde(default)]
    pub changes: Vec<AgainstChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalReport {
    pub ok: bool,
    pub command: String,
    pub schema_version: u32,
    pub generated_at: String,
    pub elapsed_ms: u64,
    pub queries: usize,
    pub repeat: usize,
    pub groups_requested: Vec<String>,
    pub corpus: CorpusReport,
    pub corpus_changed: bool,
    #[serde(default)]
    pub corpus_changed_reason: Vec<String>,
    #[serde(default)]
    pub groups: Vec<GroupReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ComparisonReport>,
    #[serde(default)]
    pub rewrites: Vec<RewriteReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub against: Option<AgainstReport>,
    pub nondeterministic: usize,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl EvalReport {
    /// `0` when the run completed; `2` when the corpus changed under the run
    /// (the comparison is refused) or the report is not `ok`.
    pub fn exit_code(&self) -> u8 {
        if !self.ok || self.corpus_changed {
            2
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub queries_path: std::path::PathBuf,
    pub repeat: usize,
    pub against: Option<std::path::PathBuf>,
    pub groups: Vec<Group>,
}

/// Run the cross-check. Read-only apart from the caller's explicit `--out`.
pub fn eval_recall(config: &RecallConfig, options: &EvalOptions) -> Result<EvalReport, EvalError> {
    if options.repeat == 0 || options.repeat > EVAL_REPEAT_MAX {
        return Err(EvalError::runtime(format!(
            "--repeat must be in 1..={EVAL_REPEAT_MAX}"
        )));
    }
    if options.groups.is_empty() {
        return Err(EvalError::runtime("--groups must name at least one group"));
    }
    let started = Instant::now();
    let text = std::fs::read_to_string(&options.queries_path).map_err(|error| {
        EvalError::runtime(format!(
            "question set {} cannot be read: {error}",
            options.queries_path.display()
        ))
    })?;
    let queries = parse_queries(&text)?;
    // The pause marker is acquired before any expensive work (including
    // `--against` parsing), so an error path still cleans it up in `Drop`.
    let pause_root = crate::eval_pause::configured_cache_root(config);
    let acquired = crate::eval_pause::acquire(
        pause_root.as_deref(),
        crate::eval_pause::now_unix_ms(),
    )
    .map_err(|error| match error {
        crate::eval_pause::PauseError::InProgress {
            pid,
            acquired_at_unix_ms,
        } => EvalError::coded(
            "EVAL_IN_PROGRESS",
            format!(
                "another eval-recall is running (pid={pid}, acquired_at_unix_ms={acquired_at_unix_ms}); \
                 the pause marker is {}",
                pause_root
                    .as_deref()
                    .map(|root| crate::eval_pause::marker_path(root).display().to_string())
                    .unwrap_or_default()
            ),
        ),
        crate::eval_pause::PauseError::Io(message) => EvalError::runtime(message),
    })?;
    let _pause_guard = acquired.guard;
    let prior = match options.against.as_deref() {
        None => None,
        Some(path) => Some(read_prior_report(path)?),
    };

    let probe_sources: Vec<&SourceConfig> = config.sources.iter().filter(|s| s.enabled).collect();
    let before = probe_corpus(config, &probe_sources);

    let mut groups: Vec<GroupReport> = Vec::new();
    for group in &options.groups {
        groups.push(run_group(config, *group, &queries, options.repeat)?);
    }

    let after = probe_corpus(config, &probe_sources);
    let mut corpus = corpus_report(&before, &after);
    let mut corpus_changed_reason: Vec<String> = Vec::new();
    if !corpus.changed.is_empty() {
        corpus_changed_reason.push("run".to_string());
    }

    // `--against` compares two points in time: the prior report's corpus and
    // this run's starting corpus must be the same, otherwise the delta would
    // mix a corpus change into a retrieval change.
    let against_change = match &prior {
        Some(prior) => {
            let changed = prior_corpus_changes(prior, &before);
            if !changed.is_empty() {
                corpus_changed_reason.push("against".to_string());
                corpus.changed.extend(changed);
                corpus.changed.sort();
                corpus.changed.dedup();
                None
            } else {
                Some(build_against(prior, &groups, options)?)
            }
        }
        None => None,
    };
    let corpus_changed = !corpus.changed.is_empty();

    let comparison = build_comparison(&groups);
    let rewrites = build_rewrites(&queries, &groups);
    let nondeterministic: usize = groups.iter().map(|group| group.nondeterministic).sum();

    let mut notes = Vec::new();
    if acquired.stale_replaced {
        notes.push(
            "stale_eval_pause: an abandoned pause marker was replaced after \
             the six-hour ceiling"
                .to_string(),
        );
    }
    if corpus.untracked > 0 {
        notes.push(
            "kb sources carry no built-in corpus fingerprint: their bytes are disclosed but never \
             judged"
                .to_string(),
        );
    }
    if corpus.unavailable > 0 {
        notes.push(
            "a live source without a readable index has no `indexed_at`/`rows` fingerprint and is \
             reported as unavailable, not as unchanged"
                .to_string(),
        );
    }
    if !corpus_changed {
        notes.push(
            "corpus fingerprints were identical before and after the run (tracked sources only)"
                .to_string(),
        );
    }

    Ok(EvalReport {
        ok: !corpus_changed,
        command: EVAL_COMMAND.to_string(),
        schema_version: EVAL_SCHEMA_VERSION,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        elapsed_ms: started.elapsed().as_millis() as u64,
        queries: queries.len(),
        repeat: options.repeat,
        groups_requested: options.groups.iter().map(|g| g.as_str().to_string()).collect(),
        corpus,
        corpus_changed,
        corpus_changed_reason,
        groups,
        comparison,
        rewrites,
        against: against_change,
        nondeterministic,
        notes,
    })
}

fn read_prior_report(path: &Path) -> Result<EvalReport, EvalError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        EvalError::runtime(format!(
            "--against {} cannot be read: {error}",
            path.display()
        ))
    })?;
    let report: EvalReport = serde_json::from_str(&text).map_err(|error| {
        EvalError::runtime(format!(
            "--against {} is not an eval-recall report: {error}",
            path.display()
        ))
    })?;
    if report.command != EVAL_COMMAND || report.schema_version != EVAL_SCHEMA_VERSION {
        return Err(EvalError::runtime(format!(
            "--against {} is not an eval-recall report at schema version {EVAL_SCHEMA_VERSION}",
            path.display()
        )));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// One group
// ---------------------------------------------------------------------------

fn run_group(
    config: &RecallConfig,
    group: Group,
    queries: &[EvalQuery],
    repeat: usize,
) -> Result<GroupReport, EvalError> {
    let sources = group_sources(config, group, None);
    if sources.is_empty() {
        return Ok(GroupReport {
            group: group.as_str().to_string(),
            status: "skipped_no_sources".to_string(),
            sources,
            metrics: None,
            by_kind: BTreeMap::new(),
            queries: Vec::new(),
            nondeterministic: 0,
        });
    }

    let mut results: Vec<QueryResult> = Vec::with_capacity(queries.len());
    let mut nondeterministic = 0usize;
    for query in queries {
        let effective = group_sources(config, group, query.sources.as_deref());
        if effective.is_empty() {
            results.push(QueryResult {
                id: query.id.clone(),
                kind: query.kind.clone(),
                negative: query.negative,
                status: "unavailable".to_string(),
                hit: None,
                rank: None,
                elapsed_ms: 0,
                source_status_entries: 0,
                source_status_non_ok: 0,
                errors: 0,
                top: Vec::new(),
                nondeterministic: false,
                differences: Vec::new(),
            });
            continue;
        }
        let request = build_request(query, &effective)?;
        let options = SearchOptions {
            strict_snapshot: false,
            eval_freeze_decay: true,
        };
        // The first run supplies the metrics; every extra run must agree on
        // the result order, position by position.
        let mut first: Option<(SearchResponse, u64, Vec<String>)> = None;
        let mut differences: Vec<String> = Vec::new();
        for run in 1..=repeat {
            let started = Instant::now();
            let response = memory_search_with(config, &request, &options);
            let elapsed = started.elapsed().as_millis() as u64;
            let order: Vec<String> = response
                .results
                .iter()
                .map(|result| result.hit_id.clone())
                .collect();
            match &first {
                None => first = Some((response, elapsed, order)),
                Some((_, _, expected)) => {
                    if *expected != order {
                        differences.push(format!(
                            "run {} differs from run 1 at position {}",
                            run,
                            first_difference(expected, &order)
                        ));
                    }
                }
            }
        }
        let (response, elapsed, _) = first.expect("at least one run");
        let nondeterministic_here = !differences.is_empty();
        if nondeterministic_here {
            nondeterministic += 1;
        }
        let (hit, rank, top) = judge(query, &response);
        let source_status_non_ok = response
            .source_status
            .iter()
            .filter(|entry| entry.status != status::OK)
            .count();
        results.push(QueryResult {
            id: query.id.clone(),
            kind: query.kind.clone(),
            negative: query.negative,
            status: "ran".to_string(),
            hit: Some(hit),
            rank,
            elapsed_ms: elapsed,
            source_status_entries: response.source_status.len(),
            source_status_non_ok,
            errors: response.errors.len(),
            top,
            nondeterministic: nondeterministic_here,
            differences,
        });
    }

    let metrics = metrics_for(&results);
    let mut by_kind: BTreeMap<String, Metrics> = BTreeMap::new();
    let mut kinds: Vec<String> = results.iter().map(|r| r.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    for kind in kinds {
        let subset: Vec<QueryResult> = results
            .iter()
            .filter(|result| result.kind == kind)
            .cloned()
            .collect();
        by_kind.insert(kind, metrics_for(&subset));
    }

    Ok(GroupReport {
        group: group.as_str().to_string(),
        status: "ran".to_string(),
        sources,
        metrics: Some(metrics),
        by_kind,
        queries: results,
        nondeterministic,
    })
}

/// `None` means the question cannot run in this group (it named sources from
/// another class); the caller records it as `unavailable`.
fn build_request(query: &EvalQuery, sources: &[String]) -> Result<SearchRequest, EvalError> {
    let mut value = json!({
        "query": query.query,
        "sources": sources,
        "limit": EVAL_LIMIT,
        "snippet_bytes": EVAL_SNIPPET_BYTES,
    });
    if let Some(mode) = &query.mode {
        value["mode"] = json!(mode);
    }
    if let Some(session_id) = &query.session_id {
        value["session_id"] = json!(session_id);
    }
    SearchRequest::parse(&value).map_err(|error| {
        EvalError::runtime(format!(
            "question {:?} produced an invalid request: {}",
            query.id, error.message
        ))
    })
}

/// The enabled source ids of one group, optionally intersected with the
/// question's explicit `sources`.
fn group_sources(
    config: &RecallConfig,
    group: Group,
    explicit: Option<&[String]>,
) -> Vec<String> {
    let base: Vec<String> = config
        .sources
        .iter()
        .filter(|source| source.enabled && group.accepts(source.kind))
        .map(|source| source.source_id.clone())
        .collect();
    match explicit {
        None => base,
        Some(ids) => ids
            .iter()
            .filter(|id| base.iter().any(|candidate| candidate == *id))
            .cloned()
            .collect(),
    }
}

/// Positive: first result whose ref matches any gold item. Negative: the
/// result list is empty. `top` carries refs only (never snippet text).
fn judge(query: &EvalQuery, response: &SearchResponse) -> (bool, Option<usize>, Vec<EvalRef>) {
    if query.negative {
        return (response.results.is_empty(), None, Vec::new());
    }
    let mut first_rank: Option<usize> = None;
    let top: Vec<EvalRef> = response
        .results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let rank = index + 1;
            let hit = query
                .gold
                .iter()
                .any(|gold| gold_matches(gold, result));
            if hit && first_rank.is_none() {
                first_rank = Some(rank);
            }
            EvalRef {
                rank,
                source_id: result.source_id.clone(),
                source_kind: result.source_kind.clone(),
                rel_path: result.reference.rel_path.clone(),
                session_id: result.reference.locator.session_id.clone(),
                hit,
            }
        })
        .collect();
    (first_rank.is_some(), first_rank, top)
}

fn gold_matches(gold: &Gold, result: &SearchResult) -> bool {
    if result.source_id != gold.source_id {
        return false;
    }
    if let Some(session_id) = &gold.session_id {
        if result.reference.locator.session_id.as_deref() != Some(session_id.as_str()) {
            return false;
        }
    }
    if let Some(rel_path) = &gold.rel_path {
        if normalize_rel_path(&result.reference.rel_path) != normalize_rel_path(rel_path) {
            return false;
        }
    }
    if let Some(snippet) = &gold.snippet {
        let haystack = normalize_text(&result.snippet);
        let needle = normalize_text(snippet);
        if !needle.is_empty() && !haystack.contains(&needle) {
            return false;
        }
    }
    true
}

fn normalize_rel_path(raw: &str) -> String {
    raw.replace('\\', "/")
}

/// NFKC, whitespace removed, ASCII lower-case. The same shape on both sides, so
/// a full-width, upper-case, wrapped or line-broken snippet still matches.
fn normalize_text(raw: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    raw.nfkc()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn first_difference(expected: &[String], actual: &[String]) -> usize {
    let mut index = 0;
    while index < expected.len().min(actual.len()) {
        if expected[index] != actual[index] {
            return index + 1;
        }
        index += 1;
    }
    index + 1
}

fn metrics_for(results: &[QueryResult]) -> Metrics {
    let ran: Vec<&QueryResult> = results
        .iter()
        .filter(|result| result.status == "ran")
        .collect();
    let positives: Vec<&QueryResult> = ran.iter().copied().filter(|r| !r.negative).collect();
    let negatives: Vec<&QueryResult> = ran.iter().copied().filter(|r| r.negative).collect();
    let ranks: Vec<usize> = positives.iter().filter_map(|r| r.rank).collect();
    let at = |k: usize| {
        if positives.is_empty() {
            0.0
        } else {
            ranks.iter().filter(|rank| **rank <= k).count() as f64 / positives.len() as f64
        }
    };
    let mrr = if ranks.is_empty() {
        0.0
    } else {
        ranks
            .iter()
            .map(|rank| 1.0 / *rank as f64)
            .sum::<f64>()
            / positives.len() as f64
    };
    let negative_pass = negatives
        .iter()
        .filter(|r| r.hit == Some(true))
        .count();
    let negative_empty_rate = if negatives.is_empty() {
        0.0
    } else {
        negative_pass as f64 / negatives.len() as f64
    };
    let status_entries: usize = ran.iter().map(|r| r.source_status_entries).sum();
    let status_non_ok: usize = ran.iter().map(|r| r.source_status_non_ok).sum();
    let mut latencies: Vec<u64> = ran.iter().map(|r| r.elapsed_ms).collect();
    latencies.sort_unstable();
    Metrics {
        queries: results.len(),
        positives: positives.len(),
        negatives: negatives.len(),
        hit_at_1: at(1),
        hit_at_5: at(5),
        hit_at_10: at(10),
        mrr,
        negative_empty_rate,
        source_status_entries: status_entries,
        source_status_non_ok: status_non_ok,
        source_status_non_ok_ratio: if status_entries == 0 {
            0.0
        } else {
            status_non_ok as f64 / status_entries as f64
        },
        errors: ran.iter().map(|r| r.errors).sum(),
        latency_ms_p50: percentile(&latencies, 0.50),
        latency_ms_p95: percentile(&latencies, 0.95),
        unavailable: results.len() - ran.len(),
    }
}

/// Nearest-rank percentile over an ascending slice.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

// ---------------------------------------------------------------------------
// Comparison, rewrites and `--against`
// ---------------------------------------------------------------------------

fn build_comparison(groups: &[GroupReport]) -> Option<ComparisonReport> {
    let ran: Vec<&GroupReport> = groups
        .iter()
        .filter(|group| group.metrics.is_some())
        .collect();
    if ran.len() < 2 {
        return None;
    }
    let baseline_group = ran
        .iter()
        .find(|group| group.group == "all")
        .or_else(|| ran.first())
        .expect("non-empty");
    let baseline = baseline_group.metrics.clone().expect("metrics");
    let rows: Vec<GroupRow> = ran
        .iter()
        .map(|group| {
            let metrics = group.metrics.clone().expect("metrics");
            let delta = if group.group == baseline_group.group {
                None
            } else {
                Some(Delta {
                    hit_at_1: metrics.hit_at_1 - baseline.hit_at_1,
                    hit_at_5: metrics.hit_at_5 - baseline.hit_at_5,
                    hit_at_10: metrics.hit_at_10 - baseline.hit_at_10,
                    mrr: metrics.mrr - baseline.mrr,
                    negative_empty_rate: metrics.negative_empty_rate
                        - baseline.negative_empty_rate,
                })
            };
            GroupRow {
                group: group.group.clone(),
                positives: metrics.positives,
                negatives: metrics.negatives,
                hit_at_1: metrics.hit_at_1,
                hit_at_5: metrics.hit_at_5,
                hit_at_10: metrics.hit_at_10,
                mrr: metrics.mrr,
                negative_empty_rate: metrics.negative_empty_rate,
                delta_vs_baseline: delta,
            }
        })
        .collect();

    let mut by_kind: Vec<KindRow> = Vec::new();
    let mut kinds: Vec<String> = ran
        .iter()
        .flat_map(|group| group.by_kind.keys().cloned())
        .collect();
    kinds.sort();
    kinds.dedup();
    for kind in kinds {
        let kind_rows: Vec<GroupRow> = ran
            .iter()
            .filter_map(|group| group.by_kind.get(&kind).map(|metrics| GroupRow {
                group: group.group.clone(),
                positives: metrics.positives,
                negatives: metrics.negatives,
                hit_at_1: metrics.hit_at_1,
                hit_at_5: metrics.hit_at_5,
                hit_at_10: metrics.hit_at_10,
                mrr: metrics.mrr,
                negative_empty_rate: metrics.negative_empty_rate,
                delta_vs_baseline: None,
            }))
            .collect();
        by_kind.push(KindRow {
            kind,
            rows: kind_rows,
        });
    }
    Some(ComparisonReport {
        baseline: baseline_group.group.clone(),
        rows,
        by_kind,
    })
}

fn build_rewrites(queries: &[EvalQuery], groups: &[GroupReport]) -> Vec<RewriteReport> {
    let mut reports = Vec::new();
    for query in queries {
        let Some(target_id) = query.rewrite_of.as_deref() else {
            continue;
        };
        let Some(target) = queries.iter().find(|candidate| candidate.id == target_id) else {
            continue;
        };
        let (natural, identifier) = if target.mode.as_deref() == Some("exact")
            && query.mode.as_deref() != Some("exact")
        {
            (query, target)
        } else {
            (target, query)
        };
        let mut rows = Vec::new();
        for group in groups {
            let natural_result = group
                .queries
                .iter()
                .find(|result| result.id == natural.id);
            let identifier_result = group
                .queries
                .iter()
                .find(|result| result.id == identifier.id);
            let natural_hit = natural_result.and_then(|result| result.hit);
            let identifier_hit = identifier_result.and_then(|result| result.hit);
            let hit_delta = match (natural_hit, identifier_hit) {
                (Some(natural), Some(identifier)) => {
                    Some(i32::from(identifier) - i32::from(natural))
                }
                _ => None,
            };
            rows.push(RewriteRow {
                group: group.group.clone(),
                natural_hit,
                natural_rank: natural_result.and_then(|result| result.rank),
                identifier_hit,
                identifier_rank: identifier_result.and_then(|result| result.rank),
                hit_delta,
            });
        }
        reports.push(RewriteReport {
            natural: natural.id.clone(),
            identifier: identifier.id.clone(),
            natural_mode: natural.mode.clone().unwrap_or_else(|| "auto".to_string()),
            identifier_mode: identifier
                .mode
                .clone()
                .unwrap_or_else(|| "auto".to_string()),
            kind: query.kind.clone(),
            rows,
        });
    }
    reports
}

fn build_against(
    prior: &EvalReport,
    groups: &[GroupReport],
    options: &EvalOptions,
) -> Result<AgainstReport, EvalError> {
    // Prefer the full-run baseline, but only when this run requested it too.
    let group_name = prior
        .groups
        .iter()
        .map(|group| group.group.as_str())
        .find(|name| *name == "all" && groups.iter().any(|group| group.group == "all"))
        .or_else(|| {
            prior
                .groups
                .iter()
                .map(|group| group.group.as_str())
                .find(|name| groups.iter().any(|group| group.group == *name))
        })
        .ok_or_else(|| {
            EvalError::runtime(
                "--against has no source group in common with this run; request one of the prior \
                 report's groups",
            )
        })?;
    let current = groups
        .iter()
        .find(|group| group.group == group_name)
        .ok_or_else(|| {
            EvalError::runtime(format!(
                "--against compares group {group_name:?}, which this run did not request"
            ))
        })?;
    let prior_group = prior
        .groups
        .iter()
        .find(|group| group.group == group_name)
        .expect("group chosen from the prior report");

    let mut changes = Vec::new();
    let mut unchanged = 0usize;
    let mut missing = 0usize;
    for before in &prior_group.queries {
        let Some(after) = current.queries.iter().find(|entry| entry.id == before.id) else {
            missing += 1;
            continue;
        };
        let change = match (before.rank, after.rank) {
            (Some(before_rank), Some(after_rank)) if before_rank == after_rank => {
                unchanged += 1;
                continue;
            }
            (Some(before_rank), Some(after_rank)) => {
                changes.push(AgainstChange {
                    id: after.id.clone(),
                    kind: after.kind.clone(),
                    group: group_name.to_string(),
                    change: "rank_changed".to_string(),
                    before_rank: Some(before_rank),
                    after_rank: Some(after_rank),
                    before_hit: before.hit,
                    after_hit: after.hit,
                    rank_delta: Some(before_rank as i64 - after_rank as i64),
                });
                continue;
            }
            (None, Some(_)) => "new_hit",
            (Some(_), None) => "new_miss",
            (None, None) => {
                if before.hit == after.hit {
                    unchanged += 1;
                } else {
                    // A negative question can flip without a rank.
                    changes.push(AgainstChange {
                        id: after.id.clone(),
                        kind: after.kind.clone(),
                        group: group_name.to_string(),
                        change: if after.hit == Some(true) {
                            "new_hit".to_string()
                        } else {
                            "new_miss".to_string()
                        },
                        before_rank: None,
                        after_rank: None,
                        before_hit: before.hit,
                        after_hit: after.hit,
                        rank_delta: None,
                    });
                }
                continue;
            }
        };
        changes.push(AgainstChange {
            id: after.id.clone(),
            kind: after.kind.clone(),
            group: group_name.to_string(),
            change: change.to_string(),
            before_rank: before.rank,
            after_rank: after.rank,
            before_hit: before.hit,
            after_hit: after.hit,
            rank_delta: match (before.rank, after.rank) {
                (Some(before_rank), Some(after_rank)) => {
                    Some(before_rank as i64 - after_rank as i64)
                }
                _ => None,
            },
        });
    }

    let new_hits = changes.iter().filter(|c| c.change == "new_hit").count();
    let new_misses = changes.iter().filter(|c| c.change == "new_miss").count();
    let rank_changes = changes.iter().filter(|c| c.change == "rank_changed").count();
    let added = current
        .queries
        .iter()
        .filter(|entry| !prior_group.queries.iter().any(|before| before.id == entry.id))
        .count();
    Ok(AgainstReport {
        path: options
            .against
            .as_deref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
        generated_at: prior.generated_at.clone(),
        group: group_name.to_string(),
        new_hits,
        new_misses,
        rank_changes,
        unchanged,
        missing,
        added,
        changes,
    })
}

// ---------------------------------------------------------------------------
// Corpus fingerprint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CorpusProbe {
    source_id: String,
    kind: String,
    status: String,
    value: Option<String>,
    reason: Option<String>,
}

fn probe_corpus(config: &RecallConfig, sources: &[&SourceConfig]) -> Vec<CorpusProbe> {
    sources
        .iter()
        .map(|source| match source.kind {
            SourceKind::Kb => CorpusProbe {
                source_id: source.source_id.clone(),
                kind: source.kind.as_str().to_string(),
                status: "untracked".to_string(),
                value: None,
                reason: Some("kb_has_no_builtin_fingerprint".to_string()),
            },
            SourceKind::Archive => match archive_generation(Path::new(&source.root)) {
                Some(value) => CorpusProbe {
                    source_id: source.source_id.clone(),
                    kind: source.kind.as_str().to_string(),
                    status: "tracked".to_string(),
                    value: Some(value),
                    reason: None,
                },
                None => CorpusProbe {
                    source_id: source.source_id.clone(),
                    kind: source.kind.as_str().to_string(),
                    status: "unavailable".to_string(),
                    value: None,
                    reason: Some("no_readable_derived_generation".to_string()),
                },
            },
            _ => match coverage::source_coverage(config, source) {
                Some(coverage) => {
                    let indexed_at = coverage
                        .get("indexed_at")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let rows = coverage
                        .get("rows")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    CorpusProbe {
                        source_id: source.source_id.clone(),
                        kind: source.kind.as_str().to_string(),
                        status: "tracked".to_string(),
                        value: Some(format!("{indexed_at}|rows={rows}")),
                        reason: None,
                    }
                }
                None => CorpusProbe {
                    source_id: source.source_id.clone(),
                    kind: source.kind.as_str().to_string(),
                    status: "unavailable".to_string(),
                    value: None,
                    reason: Some("no_readable_index".to_string()),
                },
            },
        })
        .collect()
}

/// The archive's search generation (`derived/state.json`: highest generation
/// plus its status). It changes when a replay generation is rebuilt, which is
/// exactly the evidence set the archive provider answers from.
fn archive_generation(root: &Path) -> Option<String> {
    let raw = std::fs::read(root.join("derived").join("state.json")).ok()?;
    let state: Value = serde_json::from_slice(&raw).ok()?;
    let generations = state.get("generations")?.as_array()?;
    let generation = generations.iter().max_by_key(|generation| {
        generation
            .get("generation")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    })?;
    let number = generation
        .get("generation")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let status = generation
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Some(format!("g{number}:{status}"))
}

fn corpus_report(before: &[CorpusProbe], after: &[CorpusProbe]) -> CorpusReport {
    let mut report = CorpusReport::default();
    for probe in before {
        let counterpart = after
            .iter()
            .find(|candidate| candidate.source_id == probe.source_id);
        let after_value = counterpart.and_then(|entry| entry.value.clone());
        let changed = match (&probe.status[..], counterpart.map(|entry| entry.status.as_str())) {
            ("tracked", Some("tracked")) => probe.value != after_value,
            ("tracked", _) => true,
            _ => false,
        };
        match probe.status.as_str() {
            "tracked" => report.tracked += 1,
            "untracked" => report.untracked += 1,
            _ => report.unavailable += 1,
        }
        if changed {
            report.changed.push(probe.source_id.clone());
        }
        report.sources.push(CorpusSource {
            source_id: probe.source_id.clone(),
            kind: probe.kind.clone(),
            status: probe.status.clone(),
            before: probe.value.clone(),
            after: after_value,
            changed,
            reason: probe.reason.clone(),
        });
    }
    report
}

/// Compare the prior report's end-of-run fingerprint with this run's starting
/// fingerprint. Only tracked sources are comparable; kb is never judged.
fn prior_corpus_changes(prior: &EvalReport, before: &[CorpusProbe]) -> Vec<String> {
    let mut changed = Vec::new();
    for source in &prior.corpus.sources {
        if source.status != "tracked" {
            continue;
        }
        let Some(current) = before.iter().find(|probe| probe.source_id == source.source_id) else {
            changed.push(source.source_id.clone());
            continue;
        };
        if current.status != "tracked" || source.after != current.value {
            changed.push(source.source_id.clone());
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(id: &str, status: &str, value: Option<&str>) -> CorpusProbe {
        CorpusProbe {
            source_id: id.to_string(),
            kind: "opencode".to_string(),
            status: status.to_string(),
            value: value.map(str::to_string),
            reason: None,
        }
    }

    #[test]
    fn sessions_group_includes_hermes() {
        assert!(Group::Sessions.accepts(SourceKind::Hermes));
        assert!(!Group::Kb.accepts(SourceKind::Hermes));
    }

    #[test]
    fn normalization_folds_width_case_and_whitespace() {
        assert_eq!(normalize_text("  ＡＬＰＨＡ   beta \n"), "alphabeta");
        assert!(normalize_text("缓  存\n命中率").contains(&normalize_text("缓存命中率")));
    }

    #[test]
    fn a_leading_bom_is_stripped_before_the_first_line() {
        let line = r#"{"id":"q","query":"q","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"a.md"}],"negative":false}"#;
        let queries = parse_queries(&format!("\u{feff}{line}\n")).expect("BOM is not an error");
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].id, "q");
    }

    #[test]
    fn corpus_comparison_judges_tracked_sources_only() {
        let before = vec![
            probe("oc", "tracked", Some("2026-11-07T00:00:00Z|rows=3")),
            probe("kb", "untracked", None),
            probe("arc", "unavailable", None),
        ];
        let after = vec![
            probe("oc", "tracked", Some("2026-11-07T00:00:00Z|rows=3")),
            probe("kb", "untracked", None),
            probe("arc", "unavailable", None),
        ];
        let report = corpus_report(&before, &after);
        assert!(report.changed.is_empty());
        assert_eq!(report.tracked, 1);
        assert_eq!(report.untracked, 1);
        assert_eq!(report.unavailable, 1);

        let after = vec![
            probe("oc", "tracked", Some("2026-11-07T00:00:05Z|rows=4")),
            probe("kb", "untracked", None),
            probe("arc", "unavailable", None),
        ];
        let report = corpus_report(&before, &after);
        assert_eq!(report.changed, vec!["oc".to_string()]);
    }

    #[test]
    fn percentile_uses_the_nearest_rank() {
        let values = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(percentile(&values, 0.50), 5);
        assert_eq!(percentile(&values, 0.95), 10);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn metrics_are_hand_checkable() {
        let result = |id: &str, rank: Option<usize>, hit: bool, negative: bool| QueryResult {
            id: id.to_string(),
            kind: "single_session_quote".to_string(),
            negative,
            status: "ran".to_string(),
            hit: Some(hit),
            rank,
            elapsed_ms: 1,
            source_status_entries: 1,
            source_status_non_ok: 0,
            errors: 0,
            top: Vec::new(),
            nondeterministic: false,
            differences: Vec::new(),
        };
        let results = vec![
            result("a", Some(1), true, false),
            result("b", Some(2), true, false),
            result("c", Some(4), true, false),
            result("d", None, false, false),
            result("n", None, true, true),
        ];
        let metrics = metrics_for(&results);
        assert_eq!(metrics.positives, 4);
        assert_eq!(metrics.negatives, 1);
        assert!((metrics.hit_at_1 - 0.25).abs() < 1e-9);
        assert!((metrics.hit_at_5 - 0.75).abs() < 1e-9);
        assert!((metrics.hit_at_10 - 0.75).abs() < 1e-9);
        assert!((metrics.mrr - (1.0 + 0.5 + 0.25) / 4.0).abs() < 1e-9);
        assert!((metrics.negative_empty_rate - 1.0).abs() < 1e-9);
    }
}
