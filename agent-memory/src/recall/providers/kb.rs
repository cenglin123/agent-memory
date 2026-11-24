//! KB provider: the single entry point `<kb.root>/.meta/scripts/ask.py --json`
//! (§3.4).
//!
//! Everything here is driven by the explicit configuration: the interpreter
//! (`interpreter.kb.command/args/env`), the source root, and the request
//! budget. There is no default interpreter, no `PATH` lookup, no `-X utf8`
//! injection, and no second ranking path — the child's native order is kept.
//!
//! S5.1: an explicit `mode:"exact"` request on a KB source appends `--bm25`
//! to the child call and filters the returned rows here — a row survives only
//! when its `chunk_text` contains every query term (same `tokenize` terms,
//! same ASCII-only `fold_ascii` matching as the lexical scan). The child stays
//! the retrieval authority; no BM25 logic is duplicated.
//!
//! `ask.py` only returns `path/score/chunk_index/chunk_text?`; the whole-byte
//! unique match that produces `text_range` happens here (never in Python).

use crate::recall::budget::Budget;
use crate::recall::config::{KbInterpreter, SourceConfig};
use crate::recall::contract::{
    codes, kb_hard_timeout_ms, rank_basis_for_mode, score_kind_for_mode, status, status_for_code,
    WarningEntry, KB_CONTRACT_VERSION, WARN_GRAPH_FALLBACK, WARN_RERANK_TIMEOUT,
    WARN_RERANK_UNAVAILABLE,
};
use crate::recall::hash::{file_sha256_streaming, sha256_hex, root_sha256, TextRange};
use crate::recall::providers::{SourceHit, SourceOutcome};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::snapshot::Window;
use crate::recall::text::truncate_utf8;
use serde_json::{Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Per-file ceiling for the chunk-match readback (bounded, never a full dump).
const MAX_KB_NOTE_BYTES: u64 = 64 * 1024 * 1024;
const POLL_INTERVAL_MS: u64 = 10;

/// Fixed `ask.py` exit-code → recall code/status table (§3.3). Exported so the
/// documentation-binding test can assert it instead of restating literals.
pub const ASK_EXIT_TABLE: [(i32, &str, &str); 4] = [
    (
        crate::recall::contract::ASK_EXIT_LOCAL_UNAVAILABLE,
        codes::DB_UNAVAILABLE,
        status::UNAVAILABLE,
    ),
    (
        crate::recall::contract::ASK_EXIT_PROFILE_MISMATCH,
        codes::SCHEMA_UNSUPPORTED,
        status::UNSUPPORTED,
    ),
    (
        crate::recall::contract::ASK_EXIT_REMOTE_AUTH_REQUIRED,
        codes::KB_QUERY_FAILED,
        status::ERROR,
    ),
    (2, codes::KB_QUERY_FAILED, status::ERROR),
];

/// Arguments for one KB search.
pub struct KbRequest<'a> {
    pub source: &'a SourceConfig,
    pub interpreter: &'a KbInterpreter,
    pub query: &'a str,
    pub budget: &'a Budget,
    pub snippet_bytes: usize,
    /// Directory that actually contains `.meta/scripts/ask.py`: the configured
    /// root, or a strict-snapshot copy of it.
    pub read_root: &'a Path,
    /// `false` for a throwaway snapshot: originals are never opened, so there
    /// is no observed-root window to report.
    pub observe_root: bool,
    /// S5.1: `Some(terms)` only for an explicit `mode:"exact"` request. The
    /// child call gains `--bm25` and returned rows are filtered to those whose
    /// `chunk_text` carries every term. `None` keeps the dense/hybrid call.
    pub exact_terms: Option<&'a [String]>,
    /// Stage 7 §C: `Some(triple)` only on the `eval-recall` path. The child
    /// call gains `--decay <triple>`, so the knowledge base's wall-clock time
    /// decay is frozen for a reproducible comparison. Never set by
    /// `memory_search`.
    pub frozen_decay: Option<&'a str>,
}

/// `ask.py` exit code with an envelope in hand.
pub fn map_ask_exit(exit_code: i32) -> (&'static str, &'static str) {
    ASK_EXIT_TABLE
        .iter()
        .find(|(code, _, _)| *code == exit_code)
        .map(|(_, recall_code, recall_status)| (*recall_code, *recall_status))
        .unwrap_or((codes::KB_QUERY_FAILED, status::ERROR))
}

pub fn search(request: &KbRequest<'_>) -> SourceOutcome {
    let window = if request.observe_root {
        Some(Window::open(request.read_root))
    } else {
        None
    };
    let mut outcome = run(request);
    if let Some(window) = window {
        let (side_effects, violations) = window.close();
        outcome.side_effects = side_effects;
        if !violations.is_empty() {
            outcome.violations = violations;
            outcome.status = status::ERROR;
            outcome.error_message = Some(
                "observed root changed outside the C-level runtime artifacts: \
                 known A/B path touched"
                    .to_string(),
            );
        }
    }
    outcome
}

fn failure(
    source: &SourceConfig,
    code: &'static str,
    reason: &str,
    message: String,
) -> SourceOutcome {
    let mut detail = Map::new();
    detail.insert("reason".into(), Value::String(reason.to_string()));
    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_for_code(code),
        code: Some(code),
        detail: Some(Value::Object(detail)),
        hits: Vec::new(),
        warnings: Vec::new(),
        error_message: Some(message),
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

fn run(request: &KbRequest<'_>) -> SourceOutcome {
    let source = request.source;
    let script = request
        .read_root
        .join(".meta")
        .join("scripts")
        .join("ask.py");
    if !script.is_file() {
        return failure(
            source,
            codes::SOURCE_ROOT_INVALID,
            "ask_script_missing",
            format!("ask.py is not located inside the KB root: {}", script.display()),
        );
    }

    let started = Instant::now();
    let timeout_ms = kb_hard_timeout_ms(request.budget.max_elapsed_ms_per_source);
    let args = build_args(
        &request.interpreter.args,
        &script,
        request.budget.kb_top_k,
        request.query,
        request.exact_terms.is_some(),
        request.frozen_decay,
    );
    let child = Command::new(&request.interpreter.command)
        .args(&args)
        .envs(&request.interpreter.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(err) => {
            return failure(
                source,
                codes::DB_UNAVAILABLE,
                "interpreter_spawn_failed",
                format!("KB interpreter cannot be started: {err}"),
            )
        }
    };
    let run = wait_with_timeout(child, timeout_ms);

    if run.timed_out {
        let mut detail = Map::new();
        detail.insert("code".into(), Value::String(codes::KB_TIMEOUT.to_string()));
        detail.insert(
            "budget".into(),
            serde_json::json!({
                "reason": codes::BUDGET_TIME,
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }),
        );
        return SourceOutcome {
            source_id: source.source_id.clone(),
            status: status::TRUNCATED,
            code: Some(codes::KB_TIMEOUT),
            detail: Some(Value::Object(detail)),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some(format!(
                "KB subprocess exceeded its {timeout_ms} ms hard timeout and was killed"
            )),
            side_effects: Vec::new(),
            violations: Vec::new(),
        };
    }

    let exit_code = run.status.unwrap_or(-1);
    let envelope = match parse_envelope(&run.stdout) {
        Ok(envelope) => envelope,
        Err(ParseProblem::Empty) if exit_code == 2 => {
            // `--json` is not recognised by the installed ask.py (argparse
            // rejects it and prints nothing on stdout).
            let mut detail = Map::new();
            detail.insert(
                "reason".into(),
                Value::String("ask_json_unsupported".to_string()),
            );
            return SourceOutcome {
                source_id: source.source_id.clone(),
                status: status::UNSUPPORTED,
                code: Some(codes::KB_CONTRACT_UNSUPPORTED),
                detail: Some(Value::Object(detail)),
                hits: Vec::new(),
                warnings: Vec::new(),
                error_message: Some(
                    "ask.py exited 2 without a JSON document: --json is not supported".into(),
                ),
                side_effects: Vec::new(),
                violations: Vec::new(),
            };
        }
        Err(ParseProblem::Empty) => {
            return map_child_failure(
                source,
                exit_code,
                None,
                "ask.py produced no JSON document on stdout".to_string(),
            )
        }
        Err(ParseProblem::Invalid | ParseProblem::Multiple) => {
            return map_child_failure(
                source,
                exit_code,
                None,
                "ask.py stdout is not exactly one JSON document".to_string(),
            )
        }
    };

    if exit_code != 0 {
        return map_child_failure(
            source,
            exit_code,
            Some(&envelope),
            "ask.py exited non-zero".to_string(),
        );
    }

    let observed_version = envelope.get("contract_version").and_then(Value::as_u64);
    if observed_version != Some(KB_CONTRACT_VERSION as u64) {
        // The frozen `detail` key set has no `required`/`observed` pair, so the
        // contract information is carried by `reason` plus the observed version
        // echo (§3.3 / F9 decision).
        let observed_label = observed_version
            .map(|value| value.to_string())
            .unwrap_or_else(|| "missing".to_string());
        let mut detail = Map::new();
        detail.insert(
            "kb_contract_version".into(),
            observed_version.map(Value::from).unwrap_or(Value::Null),
        );
        detail.insert(
            "reason".into(),
            Value::String(format!(
                "contract_version_required_{KB_CONTRACT_VERSION}_observed_{observed_label}"
            )),
        );
        return SourceOutcome {
            source_id: source.source_id.clone(),
            status: status::UNSUPPORTED,
            code: Some(codes::KB_CONTRACT_UNSUPPORTED),
            detail: Some(Value::Object(detail)),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some(format!(
                "KB envelope contract_version must be {KB_CONTRACT_VERSION}, observed {observed_version:?}"
            )),
            side_effects: Vec::new(),
            violations: Vec::new(),
        };
    }

    if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
        return map_child_failure(
            source,
            exit_code,
            Some(&envelope),
            "ask.py reported ok:false with exit code 0".to_string(),
        );
    }

    let vault_root = envelope
        .get("vault_root")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected_root = request.read_root.to_string_lossy().to_string();
    if normalize_root_string(vault_root) != normalize_root_string(&expected_root) {
        return failure(
            source,
            codes::KB_ROOT_MISMATCH,
            "vault_root",
            format!(
                "KB reported vault_root {vault_root:?} but the configured root is {expected_root:?}"
            ),
        );
    }

    let detail_value = envelope.get("detail").cloned().unwrap_or(Value::Null);
    let mode = detail_value
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (Some(score_kind), Some(rank_basis)) =
        (score_kind_for_mode(&mode), rank_basis_for_mode(&mode))
    else {
        let mut detail = Map::new();
        detail.insert("reason".into(), Value::String("ask_mode_invalid".to_string()));
        return SourceOutcome {
            source_id: source.source_id.clone(),
            status: status::ERROR,
            code: Some(codes::KB_QUERY_FAILED),
            detail: Some(Value::Object(detail)),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some(
                "KB envelope detail.mode is missing or outside dense|hybrid|rerank|bm25".into(),
            ),
            side_effects: Vec::new(),
            violations: Vec::new(),
        };
    };

    let mut warnings = Vec::new();
    if envelope.get("degraded").and_then(Value::as_bool) == Some(true) {
        warnings.push(WarningEntry {
            scope: "source",
            source_id: Some(source.source_id.clone()),
            hit_id: None,
            code: WARN_RERANK_UNAVAILABLE,
            detail: None,
        });
    }
    if detail_value
        .get("rerank")
        .and_then(|rerank| rerank.get("skipped"))
        .and_then(Value::as_str)
        == Some("timeout")
    {
        warnings.push(WarningEntry {
            scope: "source",
            source_id: Some(source.source_id.clone()),
            hit_id: None,
            code: WARN_RERANK_TIMEOUT,
            detail: detail_value.get("rerank").cloned(),
        });
    }
    if detail_value.get("graph_fallback").and_then(Value::as_bool) == Some(true) {
        warnings.push(WarningEntry {
            scope: "source",
            source_id: Some(source.source_id.clone()),
            hit_id: None,
            code: WARN_GRAPH_FALLBACK,
            detail: None,
        });
    }

    let root_sha = root_sha256(&source.root);
    let mut hits = Vec::new();
    let mut unresolved = 0u64;
    let rows = envelope
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for row in &rows {
        if let Some(terms) = request.exact_terms {
            if !row_carries_all_terms(row, terms) {
                // S5.1 whole-term gate: the child's BM25 recall may return
                // candidates that only match part of the query. Rows dropped
                // here are a filter decision, not a resolution failure.
                continue;
            }
        }
        match build_hit(source, &root_sha, request, row, score_kind, rank_basis) {
            Some(hit) => hits.push(hit),
            None => unresolved += 1,
        }
    }

    let mut detail = Map::new();
    detail.insert("kb_contract_version".into(), Value::from(KB_CONTRACT_VERSION));
    if let Some(escalated) = detail_value.get("escalated") {
        let items = escalated.as_array().cloned().unwrap_or_default();
        if !items.is_empty() {
            detail.insert("escalated".into(), Value::Array(items));
        }
    }
    if unresolved > 0 {
        detail.insert("CONTENT_UNRESOLVED".into(), Value::from(unresolved));
    }

    let (status_value, code, message) = if unresolved > 0 {
        (
            status::PARTIAL,
            Some(codes::CONTENT_UNRESOLVED),
            Some(format!(
                "{unresolved} KB hit(s) could not be resolved to a unique text range"
            )),
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
        detail: Some(Value::Object(detail)),
        hits,
        warnings,
        error_message: message,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

fn build_hit(
    source: &SourceConfig,
    root_sha: &str,
    request: &KbRequest<'_>,
    row: &Value,
    score_kind: &'static str,
    rank_basis: &'static str,
) -> Option<SourceHit> {
    let path = row.get("path")?.as_str()?;
    let rel_path = normalize_rel_path(path)?;
    let chunk_index = row.get("chunk_index").and_then(Value::as_u64)?;
    let chunk_text = row.get("chunk_text")?.as_str()?;
    if chunk_text.is_empty() {
        return None;
    }
    let score = row.get("score").and_then(Value::as_f64);

    let full = join_inside(request.read_root, &rel_path)?;
    let metadata = std::fs::metadata(&full).ok()?;
    if metadata.len() > MAX_KB_NOTE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&full).ok()?;
    let offset = unique_match(&bytes, chunk_text.as_bytes())?;
    let range = TextRange::from_span(&bytes, offset, offset + chunk_text.len());
    let file_sha = file_sha256_streaming(&full).ok();
    let (snippet, truncated) = truncate_utf8(chunk_text, request.snippet_bytes);
    let title = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path.as_str())
        .to_string();

    let hit = HitRef {
        source_id: source.source_id.clone(),
        source_kind: crate::recall::contract::SOURCE_KIND_KB.to_string(),
        root_sha256: root_sha.to_string(),
        rel_path: rel_path.clone(),
        schema_gen: crate::recall::contract::SCHEMA_MD_V1.to_string(),
        locator: Locator::empty(),
        fingerprint: Fingerprint {
            range_sha256: sha256_hex(chunk_text.as_bytes()),
            file_sha256: file_sha,
            row_sha256: None,
        },
        text_range: range,
    };

    Some(SourceHit {
        hit,
        title,
        snippet: snippet.to_string(),
        snippet_truncated: truncated,
        score,
        score_kind,
        rank_basis,
        distinct_terms: 0,
        first_byte: offset,
        sort_time: 0,
        sort_id: format!("{chunk_index}:{rel_path}"),
        related: Vec::new(),
                        exact: None,
    })
}

fn map_child_failure(
    source: &SourceConfig,
    exit_code: i32,
    envelope: Option<&Value>,
    message: String,
) -> SourceOutcome {
    let (code, status_value) = map_ask_exit(exit_code);
    let ask_code = envelope
        .and_then(|value| value.get("error"))
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // Frozen `detail` key set only: the ask-side code lives in `reason`, the
    // recall-side code in `code`. The child's own `error.detail.ask_code`
    // remains in its stdout for offline diagnosis.
    let mut detail = Map::new();
    detail.insert("reason".into(), Value::String(ask_code.clone()));
    detail.insert("code".into(), Value::String(code.to_string()));
    detail.insert("kb_contract_version".into(), Value::Null);
    if let Some(observed) = envelope.and_then(|value| value.get("contract_version")) {
        detail.insert("kb_contract_version".into(), observed.clone());
    }
    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code: Some(code),
        detail: Some(Value::Object(detail)),
        hits: Vec::new(),
        warnings: Vec::new(),
        error_message: Some(format!(
            "{message} (exit {exit_code}, ask code {ask_code:?})"
        )),
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ParseProblem {
    Empty,
    Invalid,
    Multiple,
}

fn parse_envelope(stdout: &[u8]) -> Result<Value, ParseProblem> {
    let trimmed = std::str::from_utf8(stdout)
        .map_err(|_| ParseProblem::Invalid)?
        .trim();
    if trimmed.is_empty() {
        return Err(ParseProblem::Empty);
    }
    let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    let first = match stream.next() {
        Some(Ok(value)) => value,
        _ => return Err(ParseProblem::Invalid),
    };
    match stream.next() {
        None => Ok(first),
        Some(_) => Err(ParseProblem::Multiple),
    }
}

fn build_args(
    configured: &[String],
    script: &Path,
    top_k: u32,
    query: &str,
    exact: bool,
    frozen_decay: Option<&str>,
) -> Vec<String> {
    let mut args = configured.to_vec();
    // The configured `args` belong to the interpreter; the script itself is the
    // single KB entry point inside the root (§3.4).
    args.push(script.to_string_lossy().to_string());
    args.push("--json".to_string());
    args.push("--no-log".to_string());
    args.push("--top-k".to_string());
    args.push(top_k.to_string());
    // S5.1: only an explicit exact request asks the child for its own lexical
    // (BM25) recall; every other mode keeps the dense/hybrid call.
    if exact {
        args.push("--bm25".to_string());
    }
    // Stage 7 §C: only the eval path freezes the child's time decay.
    if let Some(decay) = frozen_decay {
        args.push("--decay".to_string());
        args.push(decay.to_string());
    }
    // `--` ends option parsing: a legal query such as `-foo` must reach the
    // positional argument instead of being rejected by argparse (§3.3).
    args.push("--".to_string());
    args.push(query.to_string());
    args
}

/// S5.1 whole-term gate for one returned row: its `chunk_text` must contain
/// every query term under the same ASCII-only fold the lexical scan uses
/// (`tokenize` terms + `fold_ascii`; CJK terms compare as-is, no other
/// normalization). A row without `chunk_text` is not judged here — it keeps
/// the existing `CONTENT_UNRESOLVED` path instead of being silently filtered.
fn row_carries_all_terms(row: &Value, terms: &[String]) -> bool {
    match row.get("chunk_text").and_then(Value::as_str) {
        Some(text) => {
            let folded = crate::recall::text::fold_ascii(text);
            terms.iter().all(|term| folded.contains(term.as_str()))
        }
        None => true,
    }
}

struct RunOutput {
    status: Option<i32>,
    timed_out: bool,
    stdout: Vec<u8>,
}

fn wait_with_timeout(mut child: Child, timeout_ms: u64) -> RunOutput {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || read_all(stdout));
    let stderr_reader = std::thread::spawn(move || read_all(stderr));

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = exit.code();
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            }
            Err(_) => break,
        }
    }
    let stdout = stdout_reader.join().unwrap_or_default();
    // Diagnostics are never parsed; the reader is joined so the pipe is drained.
    let _ = stderr_reader.join();

    RunOutput {
        status: if timed_out { None } else { status },
        timed_out,
        stdout,
    }
}

fn read_all(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buffer);
    }
    buffer
}

fn normalize_root_string(raw: &str) -> String {
    crate::recall::hash::normalize_root(raw)
}

/// `rel_path` is always `/`-separated, relative and inside the root (§3.2).
fn normalize_rel_path(raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.contains(':')
        || normalized.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(normalized)
}

fn join_inside(root: &Path, rel_path: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(root);
    for part in rel_path.split('/') {
        path.push(part);
    }
    Some(path)
}

/// Exactly one occurrence, or nothing. Never "take the first".
fn unique_match(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let width = needle.len();
    let first = haystack.windows(width).position(|window| window == needle)?;
    if haystack[first + 1..]
        .windows(width)
        .any(|window| window == needle)
    {
        return None;
    }
    Some(first)
}

/// Number of hits that were dropped because they had no readable range.
pub fn unresolved_hits(outcome: &SourceOutcome) -> u64 {
    outcome
        .detail
        .as_ref()
        .and_then(|detail| detail.get("CONTENT_UNRESOLVED"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_table_matches_the_documented_mapping() {
        assert_eq!(
            map_ask_exit(12),
            (codes::DB_UNAVAILABLE, status::UNAVAILABLE)
        );
        assert_eq!(
            map_ask_exit(13),
            (codes::SCHEMA_UNSUPPORTED, status::UNSUPPORTED)
        );
        assert_eq!(map_ask_exit(16), (codes::KB_QUERY_FAILED, status::ERROR));
        assert_eq!(map_ask_exit(2), (codes::KB_QUERY_FAILED, status::ERROR));
        for unlisted in [1, 14, 15, 99, -1] {
            assert_eq!(
                map_ask_exit(unlisted),
                (codes::KB_QUERY_FAILED, status::ERROR)
            );
        }
    }

    #[test]
    fn hard_timeout_formula() {
        assert_eq!(kb_hard_timeout_ms(4_000), 2_000);
        assert_eq!(kb_hard_timeout_ms(20_000), 18_000);
        assert_eq!(kb_hard_timeout_ms(600_000), 18_000);
        assert_eq!(kb_hard_timeout_ms(1_000), 1_000);
    }

    #[test]
    fn unique_match_rejects_zero_and_two_occurrences() {
        assert_eq!(unique_match(b"x needle y", b"needle"), Some(2));
        assert_eq!(unique_match(b"x y z", b"needle"), None);
        assert_eq!(unique_match(b"needle needle", b"needle"), None);
        assert_eq!(unique_match(b"aaa", b"aa"), None, "overlapping counts as two");
        assert_eq!(unique_match(b"anything", b""), None);
    }

    #[test]
    fn envelope_parsing_requires_exactly_one_document() {
        assert_eq!(parse_envelope(b"").unwrap_err(), ParseProblem::Empty);
        assert_eq!(parse_envelope(b"not json").unwrap_err(), ParseProblem::Invalid);
        assert_eq!(
            parse_envelope(b"{\"a\":1}\n{\"b\":2}").unwrap_err(),
            ParseProblem::Multiple
        );
        assert!(parse_envelope(b" {\"a\":1} \n").is_ok());
    }

    #[test]
    fn rel_paths_stay_inside_the_root() {
        assert_eq!(
            normalize_rel_path("notes/a.md").as_deref(),
            Some("notes/a.md")
        );
        assert_eq!(
            normalize_rel_path("notes\\a.md").as_deref(),
            Some("notes/a.md")
        );
        for bad in ["../a.md", "/a.md", "C:/a.md", "", "a//b.md", "./a.md"] {
            assert!(normalize_rel_path(bad).is_none(), "{bad} must be rejected");
        }
    }

    #[test]
    fn build_args_puts_interpreter_args_first_and_maps_kb_top_k() {
        let configured = vec!["-B".to_string()];
        let args = build_args(
            &configured,
            Path::new("C:/root/.meta/scripts/ask.py"),
            37,
            "查询 query",
            false,
            None,
        );
        assert_eq!(
            args,
            vec![
                "-B",
                "C:/root/.meta/scripts/ask.py",
                "--json",
                "--no-log",
                "--top-k",
                "37",
                "--",
                "查询 query",
            ]
        );
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"--no-log".to_string()));
        assert!(!args.iter().any(|arg| arg == "--log"));
    }

    #[test]
    fn build_args_appends_bm25_only_for_exact_requests() {
        let exact = build_args(
            &[],
            Path::new("C:/root/.meta/scripts/ask.py"),
            5,
            "stub",
            true,
            None,
        );
        // The documented `--json --no-log --top-k <n>` prefix stays contiguous;
        // `--bm25` is appended before the terminator.
        assert_eq!(
            exact,
            vec![
                "C:/root/.meta/scripts/ask.py",
                "--json",
                "--no-log",
                "--top-k",
                "5",
                "--bm25",
                "--",
                "stub",
            ]
        );
        let plain = build_args(
            &[],
            Path::new("C:/root/.meta/scripts/ask.py"),
            5,
            "stub",
            false,
            None,
        );
        assert!(!plain.contains(&"--bm25".to_string()));
    }

    #[test]
    fn build_args_appends_the_frozen_decay_only_when_requested() {
        let frozen = build_args(
            &[],
            Path::new("C:/root/.meta/scripts/ask.py"),
            5,
            "stub",
            true,
            Some(crate::recall::contract::KB_EVAL_FROZEN_DECAY),
        );
        assert_eq!(
            frozen,
            vec![
                "C:/root/.meta/scripts/ask.py",
                "--json",
                "--no-log",
                "--top-k",
                "5",
                "--bm25",
                "--decay",
                "1.0,1.0,1.0",
                "--",
                "stub",
            ]
        );
        let plain = build_args(
            &[],
            Path::new("C:/root/.meta/scripts/ask.py"),
            5,
            "stub",
            false,
            None,
        );
        assert!(!plain.contains(&"--decay".to_string()));
    }

    #[test]
    fn whole_term_gate_keeps_only_rows_carrying_every_tokenized_term() {
        use crate::recall::text::tokenize;
        let terms = tokenize("AlphaBETA gamma");
        assert_eq!(terms, vec!["alphabeta", "gamma"]);
        // ASCII case folding on the row side, multi-word AND.
        assert!(row_carries_all_terms(&json_row("ALPHABETA gamma body"), &terms));
        // Missing either term drops the row.
        assert!(!row_carries_all_terms(&json_row("only alpha"), &terms));
        assert!(!row_carries_all_terms(&json_row("gamma alone"), &terms));

        // CJK contiguous segment: the whole phrase plus adjacent bigrams must
        // all occur, which only the full segment satisfies.
        let cjk = tokenize("统一回忆");
        assert!(row_carries_all_terms(&json_row("前文统一回忆后文"), &cjk));
        assert!(!row_carries_all_terms(&json_row("只有统一没有回忆"), &cjk));

        // No `chunk_text`: not judged here (keeps CONTENT_UNRESOLVED).
        assert!(row_carries_all_terms(&serde_json::json!({"path": "a.md"}), &terms));
    }

    fn json_row(chunk_text: &str) -> Value {
        serde_json::json!({ "path": "notes/a.md", "chunk_text": chunk_text })
    }

    #[test]
    fn build_args_terminates_options_before_a_dash_query() {
        let args = build_args(
            &[],
            Path::new("C:/root/.meta/scripts/ask.py"),
            5,
            "-foo",
            false,
            None,
        );
        let terminator = args.iter().position(|arg| arg == "--").expect("-- present");
        assert_eq!(args[terminator + 1], "-foo");
        assert_eq!(terminator, args.len() - 2, "-- directly precedes the query");
    }
}
