//! Stage 7 §C: `eval-recall` (unified retrieval cross-check).
//!
//! Every fixture is synthetic and lives in a fresh temporary directory. No
//! real question set, session root, knowledge base, index cache, Archive or
//! learning store is read or written.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use agent_memory::recall::config::load_config;
use agent_memory::recall::eval::{parse_queries, EvalError, EVAL_SCHEMA_VERSION};

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

struct Run {
    code: i32,
    stdout: Value,
    stderr: String,
}

fn run_eval(args: &[&str]) -> Run {
    let output = cli().arg("eval-recall").args(args).output().expect("run eval");
    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stdout = serde_json::from_str(&stdout_text).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    Run {
        code: output.status.code().expect("exit code"),
        stdout,
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn run_cli(args: &[&str]) -> (i32, Value, String) {
    let output = cli().args(args).output().expect("run cli");
    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stdout = if stdout_text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&stdout_text).unwrap_or(Value::Null)
    };
    (
        output.status.code().expect("exit code"),
        stdout,
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn python_exe() -> String {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            for name in ["python.exe", "python3.exe"] {
                let candidate = Path::new(dir).join(name);
                if candidate.is_file() {
                    return candidate.to_string_lossy().to_string();
                }
            }
        }
    }
    "python".to_string()
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory");
    }
    fs::write(path, text).expect("write fixture");
}

/// Synthetic `ask.py`: rows come from `<root>/.meta/behavior.json`
/// (`{query: [{path, chunk_text}]}`), in the listed order. A `counter` file
/// makes a specific query alternate its order (the `--repeat` mismatch case).
const RANK_STUB: &str = r#"import json, sys
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
BEHAVIOR = ROOT / '.meta' / 'behavior.json'
COUNTER = ROOT / '.meta' / 'counter.txt'

def rows_for(query):
    try:
        behavior = json.loads(BEHAVIOR.read_text(encoding='utf-8'))
    except (OSError, ValueError):
        behavior = {}
    rows = behavior.get(query, [])
    if query == 'vary':
        try:
            count = int(COUNTER.read_text(encoding='utf-8'))
        except (OSError, ValueError):
            count = 0
        COUNTER.write_text(str(count + 1), encoding='utf-8')
        if count % 2 == 1:
            rows = list(reversed(rows))
    out = []
    for index, row in enumerate(rows):
        out.append({
            'path': row['path'],
            'score': 1.0 - index * 0.01,
            'chunk_index': 0,
            'chunk_text': row['chunk_text'],
        })
    return out

def main(argv):
    query = ''
    index = 0
    while index < len(argv):
        token = argv[index]
        if token == '--':
            if index + 1 < len(argv):
                query = argv[index + 1]
            break
        if token in ('--top-k', '--decay'):
            index += 2
            continue
        if not token.startswith('-'):
            query = token
        index += 1
    payload = {
        'contract_version': 1,
        'ok': True,
        'vault_root': str(ROOT),
        'degraded': False,
        'detail': {'mode': 'dense', 'escalated': []},
        'results': rows_for(query),
    }
    sys.stdout.write(json.dumps(payload, ensure_ascii=False))
    sys.stdout.write('\n')
    sys.stdout.flush()
    raise SystemExit(0)

if __name__ == '__main__':
    main(sys.argv[1:])
"#;

const SHARED_STUB: &str = include_str!("fixtures/recall/kb_stub/ask.py");

struct Fixture {
    env: support::TestEnv,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let env = support::TestEnv::new();
        let cache = env.root("cache");
        Fixture { env, cache }
    }

    fn kb_root(&self, name: &str) -> PathBuf {
        let root = self.env.root(name);
        write(&root.join(".meta/scripts/ask.py"), RANK_STUB);
        root
    }

    fn install_shared_stub(&self, root: &Path) {
        write(&root.join(".meta/scripts/ask.py"), SHARED_STUB);
    }

    fn config(
        &self,
        name: &str,
        sources: Vec<Value>,
        order: Vec<&str>,
        stub: &str,
    ) -> PathBuf {
        self.env.write_json(
            &format!("{name}.json"),
            &json!({
                "version": 2,
                "domain": "default",
                "sources": sources,
                "source_order": order,
                "cache_root": self.cache.to_string_lossy(),
                "interpreter": {
                    "kb": {
                        "command": python_exe(),
                        "args": ["-B"],
                        "env": {
                            "PYTHONUTF8": "1",
                            "PYTHONDONTWRITEBYTECODE": "1",
                            "KB_STUB": stub
                        }
                    }
                }
            }),
        )
    }

    fn write_queries(&self, name: &str, lines: &[&str]) -> PathBuf {
        self.env.write(&format!("{name}.jsonl"), lines.join("\n").as_bytes())
    }
}

fn kb_line(query: &str, gold_source: &str, rel_path: &str) -> String {
    json!({
        "id": query,
        "query": query,
        "kind": "kb_multi_doc",
        "gold": [{"source_id": gold_source, "rel_path": rel_path}],
        "negative": false,
    })
    .to_string()
}

fn eval_args<'a>(config: &'a str, queries: &'a str) -> Vec<&'a str> {
    vec!["--config", config, "--queries", queries]
}

// ---------------------------------------------------------------------------
// Eval pause marker
// ---------------------------------------------------------------------------

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn marker_path(fixture: &Fixture) -> PathBuf {
    fixture.cache.join("locks").join("eval-pause.lock")
}

fn write_marker(fixture: &Fixture, acquired_at_unix_ms: i64) {
    let path = marker_path(fixture);
    fs::create_dir_all(path.parent().expect("locks dir")).expect("locks dir");
    fs::write(
        &path,
        format!("pid=999999 acquired_at_unix_ms={acquired_at_unix_ms}\n"),
    )
    .expect("pause marker");
}

// ---------------------------------------------------------------------------
// Question-set validation
// ---------------------------------------------------------------------------

#[test]
fn invalid_question_sets_are_rejected_line_by_line_without_running() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-invalid");
    write(&kb.join("notes/a.md"), "marker-a");
    let config = fixture.config(
        "invalid",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = fixture.write_queries(
        "invalid",
        &[
            // line 1: valid
            &kb_line("valid", "kb", "notes/a.md"),
            // line 2: not JSON
            "{not json",
            // line 3: missing required `negative`
            r#"{"id":"no-negative","query":"q","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"notes/a.md"}]}"#,
            // line 4: unknown kind
            r#"{"id":"bad-kind","query":"q","kind":"nonsense","gold":[{"source_id":"kb","rel_path":"notes/a.md"}],"negative":false}"#,
            // line 5: gold without an identity
            r#"{"id":"no-identity","query":"q","kind":"kb_multi_doc","gold":[{"source_id":"kb"}],"negative":false}"#,
            // line 6: negative carrying gold
            r#"{"id":"bad-negative","query":"q","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"notes/a.md"}],"negative":true}"#,
            // line 7: duplicate id
            &kb_line("valid", "kb", "notes/a.md"),
        ],
    );
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let (code, stdout, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config_text,
        "--queries",
        &queries_text,
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert_eq!(stdout, Value::Null, "nothing may run");
    let error: Value = serde_json::from_str(stderr.trim()).expect("validation JSON on stderr");
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["code"], "INVALID_QUERIES");
    let lines: Vec<u64> = error["error"]["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .map(|entry| entry["line"].as_u64().expect("line"))
        .collect();
    for line in [2, 3, 4, 5, 6, 7] {
        assert!(lines.contains(&line), "line {line} must be reported: {lines:?}");
    }
    assert!(!lines.contains(&1), "the valid line is not an error");
}

#[test]
fn rewrite_of_must_name_an_existing_question() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-rewrite-invalid");
    write(&kb.join("notes/a.md"), "marker-a");
    let config = fixture.config(
        "rewrite-invalid",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = fixture.write_queries(
        "rewrite-invalid",
        &[r#"{"id":"q","query":"q","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"notes/a.md"}],"negative":false,"rewrite_of":"missing"}"#],
    );
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let (code, _, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config_text,
        "--queries",
        &queries_text,
    ]);
    assert_eq!(code, 2);
    let error: Value = serde_json::from_str(stderr.trim()).expect("validation JSON");
    assert!(error["error"]["errors"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("unknown question"));
}

#[test]
fn an_empty_question_set_is_a_usage_error() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-empty-set");
    let config = fixture.config(
        "empty-set",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = fixture.write_queries("empty-set", &["", "   "]);
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let (code, _, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config_text,
        "--queries",
        &queries_text,
    ]);
    assert_eq!(code, 2);
    assert!(stderr.contains("empty"));
}

#[test]
fn the_shipped_sample_question_set_parses() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval/queries.sample.jsonl");
    let text = fs::read_to_string(&path).expect("sample queries");
    let queries = parse_queries(&text).expect("sample parses");
    assert_eq!(queries.len(), 5);
    let kinds: Vec<&str> = queries.iter().map(|q| q.kind.as_str()).collect();
    for kind in ["single_session_quote", "cross_session_fact", "kb_multi_doc"] {
        assert!(kinds.contains(&kind), "sample covers {kind}");
    }
    assert!(queries.iter().any(|q| q.negative));
    assert!(queries.iter().any(|q| q.rewrite_of.is_some()));
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// KB-only corpus with controlled ranks: 1, 2, 4 and a miss.
fn ranked_fixture(env: &Fixture) -> (PathBuf, PathBuf) {
    let kb = env.kb_root("kb-ranks");
    let mut behavior = serde_json::Map::new();
    let docs = ["a", "b", "c", "d", "e"];
    for doc in docs {
        write(&kb.join(format!("notes/{doc}.md")), &format!("marker-{doc}"));
    }
    let row = |doc: &str| json!({"path": format!("notes/{doc}.md"), "chunk_text": format!("marker-{doc}")});
    behavior.insert("rank-one".into(), json!([row("a")]));
    behavior.insert("rank-two".into(), json!([row("a"), row("b")]));
    behavior.insert("rank-four".into(), json!([row("a"), row("b"), row("c"), row("d")]));
    behavior.insert(
        "rank-miss".into(),
        json!([row("a"), row("b"), row("c"), row("d"), row("e")]),
    );
    behavior.insert("rank-negative".into(), json!([]));
    write(
        &kb.join(".meta/behavior.json"),
        &serde_json::to_string(&Value::Object(behavior)).unwrap(),
    );
    let config = env.config(
        "ranks",
        vec![env.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = env.write_queries(
        "ranks",
        &[
            &kb_line("rank-one", "kb", "notes/a.md"),
            &kb_line("rank-two", "kb", "notes/b.md"),
            &kb_line("rank-four", "kb", "notes/d.md"),
            &kb_line("rank-miss", "kb", "notes/e.md"),
            &json!({
                "id": "rank-negative",
                "query": "rank-negative",
                "kind": "kb_multi_doc",
                "gold": [],
                "negative": true,
            })
            .to_string(),
        ],
    );
    (config, queries)
}

#[test]
fn hit_at_k_and_mrr_match_the_hand_computation() {
    let fixture = Fixture::new();
    let (config, _) = ranked_fixture(&fixture);
    let config_text = config.to_string_lossy().to_string();
    // `rank-miss` names a document the stub never returns, so the fourth
    // positive question is a clean miss.
    let queries = fixture.write_queries(
        "ranks",
        &[
            &kb_line("rank-one", "kb", "notes/a.md"),
            &kb_line("rank-two", "kb", "notes/b.md"),
            &kb_line("rank-four", "kb", "notes/d.md"),
            &kb_line("rank-miss", "kb", "notes/missing.md"),
            &json!({
                "id": "rank-negative",
                "query": "rank-negative",
                "kind": "kb_multi_doc",
                "gold": [],
                "negative": true,
            })
            .to_string(),
        ],
    );
    let queries_text = queries.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--groups",
        "kb",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["ok"], true);
    assert_eq!(run.stdout["queries"], 5);
    // `sessions` is empty for this configuration and was not requested.
    assert_eq!(run.stdout["groups"].as_array().unwrap().len(), 1);
    assert_eq!(run.stdout["comparison"], Value::Null);
    let group = &run.stdout["groups"][0];
    assert_eq!(group["group"], "kb");
    let metrics = &group["metrics"];
    let close = |actual: &Value, expected: f64| {
        (actual.as_f64().expect("number") - expected).abs() < 1e-9
    };
    assert!(close(&metrics["hit_at_1"], 0.25), "{metrics}");
    assert!(close(&metrics["hit_at_5"], 0.75), "{metrics}");
    assert!(close(&metrics["hit_at_10"], 0.75), "{metrics}");
    assert!(close(&metrics["mrr"], 0.4375), "{metrics}");
    assert!(close(&metrics["negative_empty_rate"], 1.0), "{metrics}");
    assert_eq!(metrics["positives"], 4);
    assert_eq!(metrics["negatives"], 1);
    assert_eq!(metrics["errors"], 0);
    let ranks: Vec<Value> = group["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["rank"].clone())
        .collect();
    assert_eq!(ranks, vec![json!(1), json!(2), json!(4), Value::Null, Value::Null]);
}

#[test]
fn a_negative_question_only_passes_when_the_result_list_is_empty() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-negative");
    write(&kb.join("notes/a.md"), "marker-a");
    write(
        &kb.join(".meta/behavior.json"),
        &serde_json::json!({
            "negative-empty": [],
            "negative-hit": [{"path": "notes/a.md", "chunk_text": "marker-a"}]
        })
        .to_string(),
    );
    let config = fixture.config(
        "negative",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = fixture.write_queries(
        "negative",
        &[
            r#"{"id":"empty","query":"negative-empty","kind":"kb_multi_doc","gold":[],"negative":true}"#,
            r#"{"id":"hit","query":"negative-hit","kind":"kb_multi_doc","gold":[],"negative":true}"#,
        ],
    );
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--groups",
        "kb",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let group = &run.stdout["groups"][0];
    let entries = group["queries"].as_array().unwrap();
    assert_eq!(entries[0]["hit"], true, "an empty result passes");
    assert_eq!(entries[1]["hit"], false, "a non-empty result fails");
    let rate = group["metrics"]["negative_empty_rate"]
        .as_f64()
        .expect("rate");
    assert!((rate - 0.5).abs() < 1e-9, "{rate}");
}

// ---------------------------------------------------------------------------
// Groups and rewrites
// ---------------------------------------------------------------------------

struct TwoSource {
    config: PathBuf,
    queries: PathBuf,
}

/// One Codex session source plus one synthetic KB, each answering a different
/// question: the group table must show the split.
fn two_sources(fixture: &Fixture) -> TwoSource {
    let home = fixture.env.root("codex-home");
    support::build_codex_rollout(
        &home,
        "33333333-3333-4333-8333-333333333333",
        &[
            support::codex_meta("33333333-3333-4333-8333-333333333333"),
            support::codex_user("会话标记是 sessionword-alpha。"),
        ],
    );
    let kb = fixture.kb_root("kb-two");
    write(&kb.join("notes/kb.md"), "知识库标记 kbword-beta。");
    write(
        &kb.join(".meta/behavior.json"),
        &serde_json::json!({
            "kbword-beta": [{"path": "notes/kb.md", "chunk_text": "知识库标记 kbword-beta。"}]
        })
        .to_string(),
    );
    let config = fixture.config(
        "two",
        vec![
            support::codex_source("sessions", &home, None, None),
            fixture.env.md_source("kb", "kb", &kb),
        ],
        vec!["sessions", "kb"],
        "ok",
    );
    let queries = fixture.write_queries(
        "two",
        &[
            &json!({
                "id": "session-question",
                "query": "sessionword-alpha",
                "kind": "single_session_quote",
                "gold": [{"source_id": "sessions", "session_id": "33333333-3333-4333-8333-333333333333"}],
                "negative": false,
            })
            .to_string(),
            &kb_line("kbword-beta", "kb", "notes/kb.md"),
        ],
    );
    TwoSource { config, queries }
}

#[test]
fn the_three_source_groups_form_a_difference_table() {
    let fixture = Fixture::new();
    let case = two_sources(&fixture);
    let config = case.config.to_string_lossy().to_string();
    let queries = case.queries.to_string_lossy().to_string();
    let run = run_eval(&eval_args(&config, &queries));
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let groups = run.stdout["groups"].as_array().expect("groups");
    let by_name = |name: &str| {
        groups
            .iter()
            .find(|group| group["group"] == name)
            .unwrap_or_else(|| panic!("group {name}"))
            .clone()
    };
    assert_eq!(
        by_name("sessions")["metrics"]["hit_at_1"], 0.5,
        "sessions: {}",
        run.stdout
    );
    assert_eq!(by_name("kb")["metrics"]["hit_at_1"], 0.5);
    assert_eq!(by_name("all")["metrics"]["hit_at_1"], 1.0);
    let comparison = &run.stdout["comparison"];
    assert_eq!(comparison["baseline"], "all");
    let rows = comparison["rows"].as_array().expect("comparison rows");
    assert_eq!(rows.len(), 3);
    let session_row = rows.iter().find(|row| row["group"] == "sessions").unwrap();
    assert_eq!(session_row["delta_vs_baseline"]["hit_at_1"], -0.5);
    // Per-kind rows show which question class gains from the knowledge base.
    let kinds = comparison["by_kind"].as_array().expect("by_kind");
    assert!(kinds.iter().any(|kind| kind["kind"] == "kb_multi_doc"));
    assert!(kinds.iter().any(|kind| kind["kind"] == "single_session_quote"));
}

#[test]
fn only_the_requested_groups_run() {
    let fixture = Fixture::new();
    let case = two_sources(&fixture);
    let config = case.config.to_string_lossy().to_string();
    let queries = case.queries.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config,
        "--queries",
        &queries,
        "--groups",
        "kb",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let groups = run.stdout["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["group"], "kb");
    assert_eq!(run.stdout["comparison"], Value::Null);
}

#[test]
fn a_natural_language_and_identifier_rewrite_are_compared() {
    let fixture = Fixture::new();
    let case = two_sources(&fixture);
    let config = case.config.to_string_lossy().to_string();
    // The natural-language question misses; the identifier form (exact) hits.
    let queries = fixture.write_queries(
        "rewrite",
        &[
            &json!({
                "id": "natural",
                "query": "如何定位那个标识",
                "kind": "single_session_quote",
                "gold": [{"source_id": "sessions", "session_id": "33333333-3333-4333-8333-333333333333"}],
                "negative": false,
            })
            .to_string(),
            &json!({
                "id": "identifier",
                "query": "sessionword-alpha",
                "mode": "exact",
                "kind": "single_session_quote",
                "gold": [{"source_id": "sessions", "session_id": "33333333-3333-4333-8333-333333333333"}],
                "negative": false,
                "rewrite_of": "natural",
            })
            .to_string(),
        ],
    );
    let queries_text = queries.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config,
        "--queries",
        &queries_text,
        "--groups",
        "all",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let rewrites = run.stdout["rewrites"].as_array().expect("rewrites");
    assert_eq!(rewrites.len(), 1);
    let rewrite = &rewrites[0];
    assert_eq!(rewrite["natural"], "natural");
    assert_eq!(rewrite["identifier"], "identifier");
    assert_eq!(rewrite["natural_mode"], "auto");
    assert_eq!(rewrite["identifier_mode"], "exact");
    assert_eq!(rewrite["rows"][0]["natural_hit"], false);
    assert_eq!(rewrite["rows"][0]["identifier_hit"], true);
    assert_eq!(rewrite["rows"][0]["hit_delta"], 1);
}

// ---------------------------------------------------------------------------
// KB freeze, repeat, `--against`, atomic output, read-only
// ---------------------------------------------------------------------------

fn shared_stub_fixture(fixture: &Fixture, name: &str, branch: &str) -> (String, String) {
    let kb = fixture.env.root(name);
    fixture.install_shared_stub(&kb);
    write(&kb.join("notes/alpha.md"), "stub chunk");
    let config = fixture.config(
        name,
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        branch,
    );
    let queries = fixture.write_queries(
        name,
        &[r#"{"id":"probe","query":"probe-marker","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"notes/alpha.md"}],"negative":false}"#],
    );
    (
        config.to_string_lossy().to_string(),
        queries.to_string_lossy().to_string(),
    )
}

#[test]
fn the_frozen_decay_belongs_to_the_eval_path_only() {
    // Branch `decay-frozen` fails unless the child received
    // `--decay 1.0,1.0,1.0`; branch `decay-absent` fails if it did.
    let fixture = Fixture::new();
    let (config, queries) = shared_stub_fixture(&fixture, "decay-frozen", "decay-frozen");
    let run = run_eval(&[
        "--config",
        &config,
        "--queries",
        &queries,
        "--groups",
        "kb",
    ]);
    assert_eq!(run.code, 0, "eval must send the frozen decay: {}", run.stderr);
    assert_eq!(run.stdout["groups"].as_array().unwrap().len(), 1);
    assert_eq!(
        run.stdout["groups"][0]["queries"][0]["hit"], true,
        "eval sees the KB hit: {}",
        run.stdout
    );

    // The ordinary `memory_search` must not send it: the same branch fails.
    let (_code, search, _stderr) = run_cli(&[
        "memory_search",
        "--config",
        &config,
        "--request",
        r#"{"query":"probe-marker"}"#,
    ]);
    assert_eq!(search["results"].as_array().unwrap().len(), 0, "{search}");
    assert!(
        !search["errors"].as_array().unwrap().is_empty(),
        "memory_search must not carry the eval-only flag: {search}"
    );

    // The reverse branch proves `--decay` is really absent on memory_search.
    let (config, queries) = shared_stub_fixture(&fixture, "decay-absent", "decay-absent");
    let (code, search, stderr) = run_cli(&[
        "memory_search",
        "--config",
        &config,
        "--request",
        r#"{"query":"probe-marker"}"#,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(search["results"].as_array().unwrap().len(), 1, "{search}");
    let run = run_eval(&[
        "--config",
        &config,
        "--queries",
        &queries,
        "--groups",
        "kb",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["groups"][0]["queries"][0]["hit"], false);
    assert_eq!(run.stdout["groups"][0]["metrics"]["errors"], 1);
}

#[test]
fn repeat_two_agrees_on_the_order_and_reports_a_forced_mismatch() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-repeat");
    write(&kb.join("notes/a.md"), "marker-a");
    write(&kb.join("notes/b.md"), "marker-b");
    write(
        &kb.join(".meta/behavior.json"),
        &serde_json::json!({
            "stable": [
                {"path": "notes/a.md", "chunk_text": "marker-a"},
                {"path": "notes/b.md", "chunk_text": "marker-b"}
            ],
            "vary": [
                {"path": "notes/a.md", "chunk_text": "marker-a"},
                {"path": "notes/b.md", "chunk_text": "marker-b"}
            ]
        })
        .to_string(),
    );
    let config = fixture.config(
        "repeat",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let stable = fixture.write_queries(
        "repeat-stable",
        &[r#"{"id":"stable","query":"stable","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"notes/a.md"}],"negative":false}"#],
    );
    let config_text = config.to_string_lossy().to_string();
    let stable_text = stable.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &stable_text,
        "--groups",
        "kb",
        "--repeat",
        "2",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["nondeterministic"], 0);
    assert_eq!(run.stdout["repeat"], 2);

    let vary = fixture.write_queries(
        "repeat-vary",
        &[r#"{"id":"vary","query":"vary","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"notes/a.md"}],"negative":false}"#],
    );
    let vary_text = vary.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &vary_text,
        "--groups",
        "kb",
        "--repeat",
        "2",
    ]);
    // A nondeterministic order is reported, not fatal.
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["nondeterministic"], 1);
    assert_eq!(run.stdout["groups"][0]["queries"][0]["nondeterministic"], true);
    let differences = run.stdout["groups"][0]["queries"][0]["differences"]
        .as_array()
        .expect("differences");
    assert!(!differences.is_empty());
}

/// Build a Codex source plus a built live index (so `coverage` fingerprints).
fn indexed_session_fixture(fixture: &Fixture, name: &str) -> (String, String, PathBuf) {
    let home = fixture.env.root(name);
    support::build_codex_rollout(
        &home,
        "44444444-4444-4444-8444-444444444444",
        &[
            support::codex_meta("44444444-4444-4444-8444-444444444444"),
            support::codex_user("索引标记 index-marker-one。"),
        ],
    );
    let config = fixture.config(
        name,
        vec![support::codex_source("sessions", &home, None, None)],
        vec!["sessions"],
        "ok",
    );
    let (code, value, stderr) = run_cli(&[
        "index",
        "--config",
        &config.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "index: {value} {stderr}");
    let queries = fixture.write_queries(
        name,
        &[r#"{"id":"indexed","query":"index-marker-one","kind":"single_session_quote","gold":[{"source_id":"sessions","session_id":"44444444-4444-4444-8444-444444444444"}],"negative":false}"#],
    );
    (
        config.to_string_lossy().to_string(),
        queries.to_string_lossy().to_string(),
        home,
    )
}

#[test]
fn a_corpus_change_between_runs_refuses_the_against_comparison() {
    let fixture = Fixture::new();
    let (config, queries, home) = indexed_session_fixture(&fixture, "corpus");
    let out = fixture.env.root("reports").join("prior.json");
    let out_text = out.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config,
        "--queries",
        &queries,
        "--groups",
        "all",
        "--out",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["corpus"]["tracked"], 1);
    assert!(out.is_file());

    // Change the corpus and rebuild the index between the two runs.
    let rollout = home
        .join("sessions/2026/03/06/rollout-2026-03-06T21-50-12-44444444-4444-4444-8444-444444444444.jsonl");
    let mut text = fs::read_to_string(&rollout).expect("rollout");
    text.push_str(
        "{\"timestamp\": \"2026-03-06T21:50:15Z\", \"type\": \"response_item\", \"payload\": {\"type\": \"message\", \"role\": \"user\", \"content\": \"追加的语料 index-marker-two。\"}}\n",
    );
    fs::write(&rollout, text).expect("append rollout");
    let (code, value, stderr) = run_cli(&["index", "--config", &config]);
    assert_eq!(code, 0, "reindex: {value} {stderr}");

    let run = run_eval(&[
        "--config",
        &config,
        "--queries",
        &queries,
        "--groups",
        "all",
        "--against",
        &out_text,
    ]);
    assert_eq!(run.code, 2, "corpus change must refuse: {}", run.stdout);
    assert_eq!(run.stdout["corpus_changed"], true);
    assert_eq!(run.stdout["ok"], false);
    let reasons: Vec<&str> = run.stdout["corpus_changed_reason"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(reasons.contains(&"against"), "{reasons:?}");
    assert_eq!(run.stdout["against"], Value::Null);
}

#[test]
fn against_reports_new_hits_misses_and_rank_changes() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-against");
    write(&kb.join("notes/a.md"), "marker-a");
    write(&kb.join("notes/b.md"), "marker-b");
    let behavior = |value: Value| {
        write(
            &kb.join(".meta/behavior.json"),
            &value.to_string(),
        )
    };
    behavior(serde_json::json!({
        "rank-up": [
            {"path": "notes/a.md", "chunk_text": "marker-a"},
            {"path": "notes/b.md", "chunk_text": "marker-b"}
        ],
        "goes-missing": [
            {"path": "notes/a.md", "chunk_text": "marker-a"},
            {"path": "notes/b.md", "chunk_text": "marker-b"}
        ]
    }));
    let config = fixture.config(
        "against",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = fixture.write_queries(
        "against",
        &[
            &kb_line("rank-up", "kb", "notes/b.md"),
            &kb_line("goes-missing", "kb", "notes/b.md"),
        ],
    );
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let out = fixture.env.root("reports").join("against-prior.json");
    let out_text = out.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--groups",
        "kb",
        "--out",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["groups"][0]["queries"][0]["rank"], 2);

    // The same corpus now answers in the opposite order for one question and
    // no longer returns the other question's gold.
    behavior(serde_json::json!({
        "rank-up": [
            {"path": "notes/b.md", "chunk_text": "marker-b"},
            {"path": "notes/a.md", "chunk_text": "marker-a"}
        ],
        "goes-missing": [
            {"path": "notes/a.md", "chunk_text": "marker-a"}
        ]
    }));
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--groups",
        "kb",
        "--against",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let against = &run.stdout["against"];
    assert_eq!(against["group"], "kb");
    assert_eq!(against["new_hits"], 0);
    assert_eq!(against["new_misses"], 1);
    assert_eq!(against["rank_changes"], 1);
    let changes = against["changes"].as_array().expect("changes");
    let rank = changes
        .iter()
        .find(|change| change["id"] == "rank-up")
        .expect("rank change");
    assert_eq!(rank["change"], "rank_changed");
    assert_eq!(rank["before_rank"], 2);
    assert_eq!(rank["after_rank"], 1);
    assert_eq!(rank["rank_delta"], 1);
    let missing = changes
        .iter()
        .find(|change| change["id"] == "goes-missing")
        .expect("new miss");
    assert_eq!(missing["change"], "new_miss");
    assert_eq!(against["added"], 0);
    assert_eq!(against["missing"], 0);

    // A question set that grew since the prior report discloses the addition.
    let grown = fixture.write_queries(
        "against-grown",
        &[
            &kb_line("rank-up", "kb", "notes/b.md"),
            &kb_line("goes-missing", "kb", "notes/b.md"),
            &kb_line("brand-new", "kb", "notes/a.md"),
        ],
    );
    let grown_text = grown.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &grown_text,
        "--groups",
        "kb",
        "--against",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["against"]["added"], 1);
    assert_eq!(run.stdout["against"]["missing"], 0);
}

#[test]
fn the_report_never_copies_retrieved_text() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-bodies");
    write(&kb.join("notes/secret.md"), "body-marker-xyz secret body text");
    write(
        &kb.join(".meta/behavior.json"),
        &serde_json::json!({
            "body-marker-xyz": [{"path": "notes/secret.md", "chunk_text": "body-marker-xyz secret body text"}]
        })
        .to_string(),
    );
    let config = fixture.config(
        "bodies",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = fixture.write_queries(
        "bodies",
        &[r#"{"id":"secret","query":"body-marker-xyz","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"notes/secret.md"}],"negative":false}"#],
    );
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let out = fixture.env.root("reports").join("bodies.json");
    let out_text = out.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--groups",
        "kb",
        "--out",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["groups"][0]["queries"][0]["hit"], true);
    let stdout_text = serde_json::to_string(&run.stdout).unwrap();
    let file_text = fs::read_to_string(&out).expect("report file");
    for text in [stdout_text, file_text] {
        assert!(!text.contains("secret body text"), "retrieved text leaked");
        assert!(!text.contains("body-marker-xyz"), "the query text leaked");
        assert!(text.contains("notes/secret.md"), "the ref is present");
    }
}

#[test]
fn out_is_written_atomically_and_replaces_the_target() {
    let fixture = Fixture::new();
    let (config, queries) = ranked_fixture(&fixture);
    let reports = fixture.env.root("atomic");
    let out = reports.join("report.json");
    write(&out, "old contents");
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let out_text = out.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--groups",
        "kb",
        "--out",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let text = fs::read_to_string(&out).expect("report");
    let report: Value = serde_json::from_str(&text).expect("report is JSON");
    assert_eq!(report["command"], "eval-recall");
    assert_eq!(report["schema_version"], EVAL_SCHEMA_VERSION);
    assert!(!text.contains("old contents"));
    let leftovers: Vec<PathBuf> = fs::read_dir(&reports)
        .expect("reports dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.contains(".tmp-"))
                .unwrap_or(false)
        })
        .collect();
    assert!(leftovers.is_empty(), "temporary residue: {leftovers:?}");
}

#[test]
fn the_run_is_read_only() {
    let fixture = Fixture::new();
    let (config, queries, home) = indexed_session_fixture(&fixture, "readonly");
    let kb = fixture.env.root("kb-readonly");
    fixture.install_shared_stub(&kb);
    write(&kb.join("notes/alpha.md"), "stub chunk");
    let config_path = PathBuf::from(&config);
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config")).expect("json");
    value["sources"]
        .as_array_mut()
        .expect("sources")
        .push(fixture.env.md_source("kb", "kb", &kb));
    value["source_order"] = json!(["sessions", "kb"]);
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&value).expect("config json"),
    )
    .expect("rewrite config");
    // The added source needs its index too.
    let (code, report, stderr) = run_cli(&["index", "--config", &config]);
    assert_eq!(code, 0, "index: {report} {stderr}");

    let cache_before = support::tree_state(&fixture.cache);
    let sessions_before = support::tree_state(&home);
    let kb_before = support::tree_state(&kb);
    let out = fixture.env.root("reports-readonly").join("report.json");
    let out_text = out.to_string_lossy().to_string();
    let run = run_eval(&[
        "--config",
        &config,
        "--queries",
        &queries,
        "--out",
        &out_text,
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(support::tree_state(&fixture.cache), cache_before);
    assert_eq!(support::tree_state(&home), sessions_before);
    assert_eq!(support::tree_state(&kb), kb_before);
    assert!(
        !marker_path(&fixture).exists(),
        "the pause marker is removed before the run returns"
    );
}

/// A KB stub that keeps the child busy so the pause marker is observable
/// while the eval run is in flight.
const SLOW_STUB: &str = r#"import json, sys, time
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]

def main(argv):
    time.sleep(1.5)
    query = ''
    index = 0
    while index < len(argv):
        token = argv[index]
        if token == '--':
            if index + 1 < len(argv):
                query = argv[index + 1]
            break
        if token in ('--top-k', '--decay'):
            index += 2
            continue
        if not token.startswith('-'):
            query = token
        index += 1
    rows = []
    if query == 'slowkb-marker':
        rows = [{'path': 'notes/slow.md', 'score': 0.9, 'chunk_index': 0, 'chunk_text': 'slowkb-marker'}]
    payload = {
        'contract_version': 1,
        'ok': True,
        'vault_root': str(ROOT),
        'degraded': False,
        'detail': {'mode': 'dense', 'escalated': []},
        'results': rows,
    }
    sys.stdout.write(json.dumps(payload, ensure_ascii=False))
    sys.stdout.write('\n')
    sys.stdout.flush()
    raise SystemExit(0)

if __name__ == '__main__':
    main(sys.argv[1:])
"#;

fn slow_eval_fixture(fixture: &Fixture) -> (String, String) {
    let home = fixture.env.root("slow-home");
    support::build_codex_rollout(
        &home,
        "55555555-5555-4555-8555-555555555555",
        &[
            support::codex_meta("55555555-5555-4555-8555-555555555555"),
            support::codex_user("慢测会话标记 slow-session-marker。"),
        ],
    );
    let kb = fixture.env.root("slow-kb");
    write(&kb.join(".meta/scripts/ask.py"), SLOW_STUB);
    write(&kb.join("notes/slow.md"), "slowkb-marker");
    let config = fixture.config(
        "slow",
        vec![
            support::codex_source("sessions", &home, None, None),
            fixture.env.md_source("kb", "kb", &kb),
        ],
        vec!["sessions", "kb"],
        "ok",
    );
    let (code, report, stderr) = run_cli(&["index", "--config", &config.to_string_lossy()]);
    assert_eq!(code, 0, "index: {report} {stderr}");
    let queries = fixture.write_queries(
        "slow",
        &[
            &json!({
                "id": "slow-session",
                "query": "slow-session-marker",
                "kind": "single_session_quote",
                "gold": [{"source_id": "sessions", "session_id": "55555555-5555-4555-8555-555555555555"}],
                "negative": false,
            })
            .to_string(),
            &kb_line("slowkb-marker", "kb", "notes/slow.md"),
        ],
    );
    (
        config.to_string_lossy().to_string(),
        queries.to_string_lossy().to_string(),
    )
}

#[test]
fn index_skips_while_an_eval_pause_marker_is_live() {
    let fixture = Fixture::new();
    let (config, _queries, _home) = indexed_session_fixture(&fixture, "pause-index");
    write_marker(&fixture, now_unix_ms());
    let before = support::tree_state(&fixture.cache);

    let (code, report, stderr) = run_cli(&["index", "--config", &config]);
    assert_eq!(code, 0, "a paused refresh is not an error: {stderr}");
    assert_eq!(report["eval_pause"]["status"], "active");
    for source in report["sources"].as_array().expect("sources") {
        assert_eq!(
            source["status"], "skipped_eval_in_progress",
            "source report: {source}"
        );
        assert_eq!(source["reason"], "eval_in_progress");
        assert_eq!(source["detail"]["skipped"], "eval_in_progress");
    }
    assert_eq!(
        support::tree_state(&fixture.cache),
        before,
        "a skipped refresh writes no index bytes"
    );
    assert!(
        marker_path(&fixture).is_file(),
        "index never deletes the eval marker"
    );

    // `--source <id>` skips exactly the same way.
    let (code, report, _stderr) = run_cli(&["index", "--config", &config, "--source", "sessions"]);
    assert_eq!(code, 0);
    assert_eq!(report["sources"].as_array().unwrap().len(), 1);
    assert_eq!(report["sources"][0]["status"], "skipped_eval_in_progress");
}

#[test]
fn a_stale_pause_marker_is_ignored_and_disclosed() {
    let fixture = Fixture::new();
    let (config, _queries, _home) = indexed_session_fixture(&fixture, "pause-stale");
    write_marker(&fixture, now_unix_ms() - 7 * 60 * 60 * 1000);

    let (code, report, stderr) = run_cli(&["index", "--config", &config]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(report["eval_pause"]["status"], "stale");
    assert_eq!(report["eval_pause"]["reason"], "stale_eval_pause");
    assert!(report["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .any(|warning| warning == "stale_eval_pause"));
    for source in report["sources"].as_array().expect("sources") {
        assert_ne!(source["status"], "skipped_eval_in_progress");
    }
    assert!(
        marker_path(&fixture).is_file(),
        "only the eval guard removes the marker"
    );
}

#[test]
fn a_second_eval_is_refused_with_eval_in_progress() {
    let fixture = Fixture::new();
    let case = two_sources(&fixture);
    write_marker(&fixture, now_unix_ms());
    let config = case.config.to_string_lossy().to_string();
    let queries = case.queries.to_string_lossy().to_string();
    let (code, stdout, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config,
        "--queries",
        &queries,
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert_eq!(stdout, Value::Null, "nothing may run");
    let error: Value = serde_json::from_str(stderr.trim()).expect("error JSON");
    assert_eq!(error["error"]["code"], "EVAL_IN_PROGRESS");
    assert!(marker_path(&fixture).is_file());
}

#[test]
fn the_pause_marker_is_removed_on_success_and_on_error() {
    let fixture = Fixture::new();
    let (config, queries, _home) = indexed_session_fixture(&fixture, "pause-cleanup");
    let run = run_eval(&eval_args(&config, &queries));
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(!marker_path(&fixture).exists(), "a clean run removes its marker");

    // A runtime error after the marker was acquired must clean it up too.
    let missing = fixture.env.root("reports").join("missing-prior.json");
    let missing_text = missing.to_string_lossy().to_string();
    let (code, _, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config,
        "--queries",
        &queries,
        "--against",
        &missing_text,
    ]);
    assert_eq!(code, 2);
    let error: Value = serde_json::from_str(stderr.trim()).expect("error JSON");
    assert_eq!(error["error"]["code"], "EVAL_RECALL_FAILED");
    assert!(
        !marker_path(&fixture).exists(),
        "an error path removes its marker"
    );

    // A stale marker is replaced, disclosed, and removed at the end.
    write_marker(&fixture, now_unix_ms() - 7 * 60 * 60 * 1000);
    let run = run_eval(&eval_args(&config, &queries));
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let notes: Vec<&str> = run.stdout["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("stale_eval_pause")),
        "{notes:?}"
    );
    assert!(!marker_path(&fixture).exists());
}

#[test]
fn an_index_started_during_an_eval_run_is_skipped() {
    let fixture = Fixture::new();
    let (config, queries) = slow_eval_fixture(&fixture);
    let mut child = cli()
        .args([
            "eval-recall",
            "--config",
            &config,
            "--queries",
            &queries,
            "--repeat",
            "2",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn eval");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker_path(&fixture).is_file() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the eval pause marker never appeared");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (code, report, stderr) = run_cli(&["index", "--config", &config]);
    assert_eq!(code, 0, "the concurrent refresh exits 0: {stderr}");
    assert_eq!(report["eval_pause"]["status"], "active");
    let sources = report["sources"].as_array().expect("sources");
    let session_source = sources
        .iter()
        .find(|source| source["source_id"] == "sessions")
        .expect("sessions source");
    assert_eq!(session_source["status"], "skipped_eval_in_progress");
    assert_eq!(session_source["reason"], "eval_in_progress");
    let kb_source = sources
        .iter()
        .find(|source| source["source_id"] == "kb")
        .expect("kb source");
    assert_eq!(
        kb_source["status"], "skipped_not_indexable",
        "a KB source never had an index to pause"
    );

    let output = child.wait_with_output().expect("eval finishes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "eval stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_str(&stdout).expect("eval JSON");
    assert_eq!(value["corpus_changed"], false);
    assert_eq!(value["ok"], true);
    assert!(!marker_path(&fixture).exists());
}



#[test]
fn usage_errors_are_exit_two_and_named() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-usage");
    let config = fixture.config(
        "usage",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let queries = fixture.write_queries(
        "usage",
        &[r#"{"id":"q","query":"q","kind":"kb_multi_doc","gold":[{"source_id":"kb","rel_path":"a.md"}],"negative":false}"#],
    );
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();

    let (code, _, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--repeat",
        "0",
    ]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--repeat"));

    let (code, _, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--groups",
        "nonsense",
    ]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--groups"));

    let (code, _, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config_text,
        "--queries",
        &queries_text,
        "--bogus",
        "x",
    ]);
    assert_eq!(code, 2);
    assert!(stderr.contains("unknown option"));

    let (code, _, stderr) = run_cli(&[
        "eval-recall",
        "--config",
        &config_text,
        "--queries",
        &fixture.env.path().join("missing.jsonl").to_string_lossy(),
    ]);
    assert_eq!(code, 2);
    assert!(stderr.contains("cannot be read"));
}

#[test]
fn the_sample_fixture_runs_end_to_end() {
    let fixture = Fixture::new();
    let source_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval");
    let codex = fixture.env.root("sample-codex");
    copy_tree(&source_root.join("codex"), &codex);
    let kb = fixture.env.root("sample-kb");
    copy_tree(&source_root.join("kb"), &kb);
    let config = fixture.config(
        "sample",
        vec![
            support::codex_source("sessions", &codex, None, None),
            fixture.env.md_source("kb", "kb", &kb),
        ],
        vec!["sessions", "kb"],
        "ok",
    );
    let queries = source_root.join("queries.sample.jsonl");
    let config_text = config.to_string_lossy().to_string();
    let queries_text = queries.to_string_lossy().to_string();
    let run = run_eval(&eval_args(&config_text, &queries_text));
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout["queries"], 5);
    assert_eq!(run.stdout["groups"].as_array().unwrap().len(), 3);
    for group in run.stdout["groups"].as_array().unwrap() {
        assert_eq!(group["metrics"]["errors"], 0, "{group}");
    }
    let all = run.stdout["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["group"] == "all")
        .expect("all group");
    assert_eq!(all["metrics"]["hit_at_1"], 1.0);
    assert_eq!(all["metrics"]["negative_empty_rate"], 1.0);
    assert_eq!(run.stdout["rewrites"].as_array().unwrap().len(), 1);
    assert!(run.stdout["comparison"]["rows"].as_array().unwrap().len() >= 3);
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("target dir");
    for entry in fs::read_dir(source).expect("read fixture dir").flatten() {
        let path = entry.path();
        let destination = target.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &destination);
        } else {
            fs::copy(&path, &destination).expect("copy fixture");
        }
    }
}

// Keep the config loader reachable so the fixture shape stays the one the
// binary accepts (a compile-time link, not an unused import).
#[test]
fn the_fixture_configuration_loads_through_the_real_loader() {
    let fixture = Fixture::new();
    let kb = fixture.kb_root("kb-loader");
    let config = fixture.config(
        "loader",
        vec![fixture.env.md_source("kb", "kb", &kb)],
        vec!["kb"],
        "ok",
    );
    let loaded = load_config(&config).expect("config loads");
    assert_eq!(loaded.enabled_sources(), vec!["kb".to_string()]);
    assert!(matches!(
        parse_queries(""),
        Err(EvalError::Queries(_))
    ));
}
