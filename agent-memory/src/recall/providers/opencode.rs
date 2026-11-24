//! OpenCode provider: generation resolution, budget progress, `coverage`,
//! lexical ranking and source status. Only `text` / `/text` / `tool` parts
//! produce hits.

use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{codes, status};
use crate::recall::hash::{sha256_hex, TextRange, BLOCK_BYTES};
use crate::recall::port::blocks::{prefixes_equal, PrefixDigest, PrefixFingerprint};
use crate::recall::port::{classify_part, resolve, Health, PartOutcome};
use crate::recall::providers::{rank_hits, searchable_sessions, SourceHit, SourceOutcome};
use crate::recall::refs::{Fingerprint, HitRef};
use crate::recall::snapshot::Window;
use crate::recall::text::{fold_ascii, lexical_match, snippet, LexMatch};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Instant;

const CACHE_ENTRY_LIMIT: usize = 64 * 1024;
const CACHE_TOTAL_LIMIT: usize = 4 * 1024 * 1024;

/// Bounded prefix-fingerprint cache: identical projections reuse the lexical
/// match. A digest hit is always byte-verified; different bytes (a collision or
/// misaligned block boundary) simply recompute instead of returning a stale
/// match.
struct MatchCache {
    entries: HashMap<PrefixDigest, (Vec<u8>, Option<LexMatch>)>,
    bytes: usize,
}

impl MatchCache {
    fn new() -> Self {
        MatchCache {
            entries: HashMap::new(),
            bytes: 0,
        }
    }

    fn lookup(&mut self, projection: &str, terms: &[String]) -> Option<LexMatch> {
        let bytes = projection.as_bytes();
        let mut fingerprint = PrefixFingerprint::new();
        for block in bytes.chunks(BLOCK_BYTES) {
            fingerprint.push_block(block);
        }
        let digest = fingerprint.snapshot();
        if let Some((stored, found)) = self.entries.get(&digest) {
            if prefixes_equal(stored, bytes) {
                return found.clone();
            }
        }
        let folded = fold_ascii(projection);
        let found = lexical_match(&folded, terms);
        if bytes.len() <= CACHE_ENTRY_LIMIT && self.bytes + bytes.len() <= CACHE_TOTAL_LIMIT {
            self.bytes += bytes.len();
            self.entries.insert(digest, (bytes.to_vec(), found.clone()));
        }
        found
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Bytes,
    Time,
}

pub fn search(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
) -> SourceOutcome {
    search_at(
        source,
        terms,
        budget,
        snippet_bytes,
        None,
        Path::new(&source.root),
        true,
    )
}

/// Search reading from `read_root` (the configured root, or a strict-snapshot
/// copy of it). `root_sha256`/`rel_path` keep using the configuration string, so
/// refs are identical in both modes.
pub fn search_at(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    session_filter: Option<&str>,
    read_root: &Path,
    observe_root: bool,
) -> SourceOutcome {
    let window = if observe_root {
        Some(Window::open(read_root))
    } else {
        None
    };
    let mut outcome = scan(source, terms, budget, snippet_bytes, session_filter, read_root);
    if let Some(window) = window {
        let (side_effects, violations) = window.close();
        outcome.side_effects = side_effects;
        if !violations.is_empty() {
            outcome.violations = violations;
            outcome.status = status::ERROR;
            outcome.error_message = Some(format!(
                "observed root changed outside the C-level runtime artifacts: {:?}",
                outcome.violations
            ));
        }
    }
    outcome
}

fn remap_configured(read_root: &Path, config_root: &str, value: Option<&str>) -> Option<String> {
    let value = value?;
    if read_root == Path::new(config_root) {
        return Some(value.to_string());
    }
    // Strict snapshot: keep the same relative position inside the copy. The
    // configuration already guarantees containment; an unmappable value falls
    // back to the copy's default path instead of escaping to the original.
    let rel = crate::recall::port::reader::rel_from_root(config_root, Path::new(value))?;
    Some(read_root.join(rel).to_string_lossy().to_string())
}

fn scan(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    session_filter: Option<&str>,
    read_root: &Path,
) -> SourceOutcome {
    let root = read_root;
    // Rel paths / containment are computed against the tree we actually read
    // (the configured root, or its strict-snapshot copy). `root_sha256` keeps
    // using `source.root`, so refs are identical in both modes.
    let read_root_str = root.to_string_lossy().to_string();
    let started = Instant::now();
    let mapped_db = remap_configured(root, &source.root, source.db.as_deref());
    let mapped_storage = remap_configured(root, &source.root, source.storage.as_deref());
    let mut resolved = match resolve(
        root,
        &read_root_str,
        mapped_db.as_deref(),
        mapped_storage.as_deref(),
    ) {
        Ok(resolved) => resolved,
        Err(err) => {
            return SourceOutcome::from_port_error(
                &source.source_id,
                &err,
                Vec::new(),
                Vec::new(),
            )
        }
    };

    let mut health = Health::default();
    let sessions = match resolved.reader.sessions(&mut health) {
        Ok(sessions) => sessions,
        Err(err) => {
            return SourceOutcome::from_port_error(
                &source.source_id,
                &err,
                Vec::new(),
                Vec::new(),
            )
        }
    };
    let searchable = searchable_sessions(&sessions);
    // Filtering happens after session enumeration and before any part is read:
    // the budget only pays for the target session, and `total_sessions` counts
    // the filtered population.
    let searchable: Vec<&crate::recall::port::SessionMeta> = match session_filter {
        Some(id) => searchable
            .into_iter()
            .filter(|session| session.session_id == id)
            .collect(),
        None => searchable,
    };
    let total_sessions = searchable.len() as u64;
    let mut scanned_sessions = 0u64;
    let mut hits: Vec<SourceHit> = Vec::new();
    let mut ignored_types: BTreeMap<String, u64> = BTreeMap::new();
    let mut unresolved = 0u64;
    let mut oversize = false;
    let mut stop: Option<StopReason> = None;
    let mut cache = MatchCache::new();

    'sessions: for session in searchable {
        if health.bytes >= budget.max_bytes_per_source {
            stop = Some(StopReason::Bytes);
            break;
        }
        if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
            stop = Some(StopReason::Time);
            break;
        }
        scanned_sessions += 1;
        let parts = match resolved
            .reader
            .parts_for_session(&session.session_id, budget.max_bytes_per_source, &mut health)
        {
            Ok(parts) => parts,
            Err(err) => {
                // A single unreadable session is a partial read, never an
                // empty source.
                health.error();
                let _ = err;
                continue;
            }
        };
        for part in parts {
            if part.raw_bytes > budget.max_bytes_per_source {
                health.note_oversize();
                oversize = true;
                continue;
            }
            if health.bytes >= budget.max_bytes_per_source {
                stop = Some(StopReason::Bytes);
                break 'sessions;
            }
            if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
                stop = Some(StopReason::Time);
                break 'sessions;
            }
            match classify_part(&part.data) {
                PartOutcome::Injected => continue,
                PartOutcome::Ignored { type_literal } => {
                    if !type_literal.is_empty() {
                        *ignored_types.entry(type_literal).or_insert(0) += 1;
                    }
                    continue;
                }
                PartOutcome::Unresolved { .. } => {
                    unresolved += 1;
                    continue;
                }
                PartOutcome::Text { projection, .. } => {
                    if projection.is_empty() {
                        // Legal empty block: no unit, no segment, no error.
                        continue;
                    }
                    let Some(found) = cache.lookup(&projection, terms) else {
                        continue;
                    };
                    let text_bytes = projection.as_bytes();
                    let text_range = TextRange::from_span(text_bytes, 0, text_bytes.len());
                    let (display, truncated) = snippet(
                        &projection,
                        found.first_byte,
                        found.first_term_len,
                        snippet_bytes,
                    );
                    let hit = HitRef {
                        source_id: source.source_id.clone(),
                        source_kind: "opencode".to_string(),
                        root_sha256: crate::recall::hash::root_sha256(&source.root),
                        rel_path: part.rel_path.clone(),
                        schema_gen: resolved.generation.to_string(),
                        locator: part.locator.clone(),
                        fingerprint: Fingerprint {
                            range_sha256: sha256_hex(text_bytes),
                            file_sha256: None,
                            row_sha256: Some(part.row_sha256.clone()),
                        },
                        text_range,
                    };
                    hits.push(SourceHit {
                        hit,
                        title: session.display_title(),
                        snippet: display,
                        snippet_truncated: truncated,
                        score: None,
                        score_kind: crate::recall::contract::SCORE_KIND_NONE,
                        rank_basis: crate::recall::contract::RANK_BASIS_LEXICAL_WINDOW,
                        distinct_terms: found.distinct_terms,
                        first_byte: found.first_byte,
                        sort_time: if part.time_created != 0 {
                            part.time_created
                        } else {
                            session.time_updated
                        },
                        sort_id: format!("{}:{}", session.session_id, part.sort_id),
                        related: Vec::new(),
                        exact: None,
                    });
                }
            }
        }
    }

    rank_hits(&mut hits);

    if health.oversize > 0 {
        oversize = true;
    }

    let mut detail = Map::new();
    let mut coverage = json!({
        "scanned_sessions": scanned_sessions,
        "total_sessions": total_sessions,
    });
    if let Some(id) = session_filter {
        coverage["session_filter"] = json!(id);
    }
    detail.insert("coverage".into(), coverage);
    if !ignored_types.is_empty() {
        let map: Map<String, Value> = ignored_types
            .iter()
            .map(|(key, count)| (key.clone(), json!(count)))
            .collect();
        detail.insert("ignored_types".into(), Value::Object(map));
    }
    if let Some(state) = resolved.session_v2_state {
        detail.insert("session_v2".into(), json!(state));
    }
    if unresolved > 0 {
        detail.insert("CONTENT_UNRESOLVED".into(), json!(unresolved));
    }
    if oversize {
        detail.insert("reason".into(), json!("oversize"));
    }
    if let Some(reason) = stop {
        let mut budget_detail = Map::new();
        match reason {
            StopReason::Bytes => {
                budget_detail.insert("reason".into(), json!(codes::BUDGET_BYTES));
                budget_detail.insert("consumed_bytes".into(), json!(health.bytes));
            }
            StopReason::Time => {
                budget_detail.insert("reason".into(), json!(codes::BUDGET_TIME));
                budget_detail.insert(
                    "elapsed_ms".into(),
                    json!(started.elapsed().as_millis() as u64),
                );
            }
        }
        detail.insert("budget".into(), Value::Object(budget_detail));
    }

    let content_loss = health.skipped > 0 || health.errors > 0 || unresolved > 0;
    let (status_value, code, message) = if content_loss {
        (
            status::PARTIAL,
            Some(if health.skipped > 0 || health.errors > 0 {
                codes::RECORD_SKIPPED
            } else {
                codes::CONTENT_UNRESOLVED
            }),
            Some(format!(
                "scan lost records (skipped={}, errors={}, unresolved={})",
                health.skipped, health.errors, unresolved
            )),
        )
    } else if let Some(reason) = stop {
        (
            status::TRUNCATED,
            Some(match reason {
                StopReason::Bytes => codes::BUDGET_BYTES,
                StopReason::Time => codes::BUDGET_TIME,
            }),
            Some("source budget exhausted before the scan completed".to_string()),
        )
    } else if hits.is_empty() {
        (status::EMPTY, None, None)
    } else {
        (status::OK, None, None)
    };

    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        hits,
        warnings: Vec::new(),
        error_message: message,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}
