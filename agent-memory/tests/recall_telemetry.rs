//! P2 telemetry: append-only event storage, idempotency, external anchors,
//! counters, export, bounded retention and prune.
//!
//! Every case uses a temporary directory and synthetic fixtures only: no real
//! session, knowledge base or production Archive is read, and the event store
//! is always a temporary file.

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const CHUNK: &str = "stub chunk";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
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

fn hit(ch: char) -> String {
    ch.to_string().repeat(64)
}

fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn offset_days(days: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Write the KB stub and one note, then a v2 configuration with the given
/// `telemetry` block. Returns the configuration path and the event store path
/// declared by that block (if any).
fn config(
    env: &support::TestEnv,
    name: &str,
    telemetry: Value,
    events_path: Option<&Path>,
) -> (PathBuf, Option<PathBuf>) {
    let root = env.root(name);
    let script = root.join(".meta").join("scripts").join("ask.py");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall/kb_stub/ask.py"),
        &script,
    )
    .expect("install kb stub");
    env.write(&format!("{name}/notes/alpha.md"), CHUNK.as_bytes());

    let mut telemetry = telemetry;
    if telemetry["enabled"] == json!(true) && telemetry.get("path").is_none() {
        let path = events_path
            .map(|path| path.to_path_buf())
            .unwrap_or_else(|| {
                env.path()
                    .join(format!("{name}-state"))
                    .join("retrieval_events.sqlite")
            });
        telemetry["path"] = json!(path.to_string_lossy());
    }
    let events = telemetry
        .get("path")
        .and_then(|value| value.as_str())
        .map(PathBuf::from);

    let value = json!({
        "version": 2,
        "sources": [env.md_source("kb", "kb", &root)],
        "source_order": ["kb"],
        "interpreter": {
            "kb": {
                "command": python_exe(),
                "args": ["-B"],
                "env": {
                    "PYTHONUTF8": "1",
                    "PYTHONDONTWRITEBYTECODE": "1",
                    "KB_STUB": "ok"
                }
            }
        },
        "domain": "default",
        "telemetry": telemetry
    });
    let path = env.write_json(&format!("{name}-config.json"), &value);
    (path, events)
}

fn enabled_config(env: &support::TestEnv, name: &str) -> (PathBuf, PathBuf) {
    let (config, events) = config(env, name, json!({"enabled": true}), None);
    (config, events.expect("events path"))
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Run {
    let output = Command::new(bin()).args(args).output().expect("run CLI");
    Run {
        code: output.status.code().expect("exit code"),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn run_json(args: &[&str]) -> (i32, Value) {
    let run = run(args);
    let value = serde_json::from_str::<Value>(run.stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON:\n{}\nstderr: {}",
            run.stdout, run.stderr
        )
    });
    (run.code, value)
}

fn record(config: &Path, event: &Value) -> (i32, Value) {
    run_json(&[
        "events",
        "record",
        "--config",
        config.to_str().unwrap(),
        "--event",
        &serde_json::to_string(event).unwrap(),
    ])
}

fn prune(config: &Path) -> (i32, Value) {
    run_json(&["events", "prune", "--config", config.to_str().unwrap()])
}

fn stats(config: &Path, hit_id: Option<&str>) -> (i32, Value) {
    let mut args = vec!["events", "stats", "--config", config.to_str().unwrap()];
    if let Some(hit_id) = hit_id {
        args.push("--hit-id");
        args.push(hit_id);
    }
    run_json(&args)
}

/// `events export` writes JSON Lines to stdout and the envelope to stderr.
fn export(config: &Path, include_query: bool) -> (i32, Vec<Value>, Value) {
    let mut args = vec!["events", "export", "--config", config.to_str().unwrap()];
    if include_query {
        args.push("--include-query");
    }
    let run = run(&args);
    let lines = run
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|_| panic!("export line is not JSON: {line}"))
        })
        .collect::<Vec<_>>();
    let summary = run
        .stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .last()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or(Value::Null);
    (run.code, lines, summary)
}

fn retrieved(hit_id: &str) -> Value {
    json!({"kind": "retrieved", "hit_id": hit_id, "ts": now()})
}

fn read_event(hit_id: &str) -> Value {
    json!({"kind": "read", "hit_id": hit_id, "ts": now()})
}

fn find_hit<'a>(stats: &'a Value, hit_id: &str) -> &'a Value {
    stats["hits"]
        .as_array()
        .expect("hits array")
        .iter()
        .find(|row| row["hit_id"] == json!(hit_id))
        .unwrap_or_else(|| panic!("hit {hit_id} missing from stats: {stats}"))
}

struct McpSession {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl McpSession {
    fn start(config: &Path) -> McpSession {
        let mut child = Command::new(bin())
            .arg("mcp")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut session = McpSession {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let initialize = session.request("initialize", json!({"protocolVersion": "2024-11-05"}));
        assert_eq!(initialize["result"]["protocolVersion"], "2024-11-05");
        session
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .expect("write request");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        serde_json::from_str(&line).expect("response JSON")
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mcp_search_and_read(config: &Path) {
    let mut mcp = McpSession::start(config);
    let tools = mcp.request("tools/list", json!({}));
    for tool in tools["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let expected = if name == "memory_read" { json!(false) } else { json!(true) };
        assert_eq!(tool["annotations"]["readOnlyHint"], expected, "{name}");
    }
    let search = mcp.call("memory_search", json!({"query": "stub"}));
    assert_eq!(search["result"]["isError"], json!(false));
    assert_eq!(
        search["result"]["structuredContent"]["side_effects"],
        json!([])
    );
    let reference = search["result"]["structuredContent"]["results"][0]["ref"].clone();
    let read = mcp.call(
        "memory_read",
        json!({"ref": reference, "context_before": 0, "context_after": 0}),
    );
    assert_eq!(read["result"]["isError"], json!(false));
    assert_eq!(read["result"]["structuredContent"]["side_effects"], json!([]));
}

#[test]
fn mcp_search_and_read_never_create_or_modify_event_storage() {
    let env = support::TestEnv::new();
    let (config, events) = enabled_config(&env, "mcp-telemetry");
    assert!(!events.exists());

    mcp_search_and_read(&config);
    assert!(
        !events.exists(),
        "MCP must never create {}",
        events.display()
    );
    assert!(
        !events.parent().unwrap().exists(),
        "MCP must not create the telemetry state directory"
    );

    // Once `events record` created the store, MCP must not change a byte.
    let (code, response) = record(&config, &retrieved(&hit('a')));
    assert_eq!(code, 0, "{response}");
    assert_eq!(response["wrote"], json!(true));
    let before = std::fs::read(&events).unwrap();
    mcp_search_and_read(&config);
    let after = std::fs::read(&events).unwrap();
    assert_eq!(before, after, "MCP must not write to the event store");
}

#[test]
fn memory_search_and_read_never_produce_applied_or_confirmed() {
    let env = support::TestEnv::new();
    let (config, events) = enabled_config(&env, "search-only");
    let (code, _) = record(&config, &retrieved(&hit('a')));
    assert_eq!(code, 0);
    let before = std::fs::read(&events).unwrap();

    let search = run_json(&[
        "memory_search",
        "--config",
        config.to_str().unwrap(),
        "--request",
        &json!({"query": "stub"}).to_string(),
    ]);
    assert_eq!(search.0, 0, "{}", search.1);
    let reference = search.1["results"][0]["ref"].clone();
    let read = run_json(&[
        "memory_read",
        "--config",
        config.to_str().unwrap(),
        "--request",
        &json!({"ref": reference, "context_before": 0, "context_after": 0}).to_string(),
    ]);
    assert_eq!(read.0, 0, "{}", read.1);

    let (code, stats) = stats(&config, None);
    assert_eq!(code, 0);
    assert_eq!(stats["totals"]["events"], json!(1));
    assert_eq!(stats["totals"]["applied"], json!(0));
    assert_eq!(stats["totals"]["confirmed"], json!(0));
    assert_eq!(find_hit(&stats, &hit('a'))["read"], json!(0));
    assert_eq!(std::fs::read(&events).unwrap(), before);
}

#[test]
fn retrieved_and_read_events_are_recorded_and_counted() {
    let env = support::TestEnv::new();
    let (config, _) = enabled_config(&env, "positive-events");
    let id = hit('b');

    let (code, response) = record(&config, &retrieved(&id));
    assert_eq!(code, 0, "{response}");
    assert_eq!(response["code"], json!("EVENTS_RECORDED"));
    assert_eq!(response["wrote"], json!(true));
    assert!(response["location"].as_str().unwrap().ends_with("retrieval_events.sqlite"));

    let (code, response) = record(&config, &read_event(&id));
    assert_eq!(code, 0, "{response}");
    assert_eq!(response["wrote"], json!(true));

    let (code, stats) = stats(&config, Some(&id));
    assert_eq!(code, 0);
    let row = find_hit(&stats, &id);
    assert_eq!(row["retrieved"], json!(1));
    assert_eq!(row["read"], json!(1));
    assert_eq!(row["applied"], json!(0));
    assert_eq!(row["confirmed"], json!(0));
    assert_eq!(row["events"], json!(2));
    assert_eq!(stats["totals"]["events"], json!(2));
    assert_eq!(stats["totals"]["hits"], json!(1));

    let (code, lines, summary) = export(&config, false);
    assert_eq!(code, 0);
    assert_eq!(summary["exported"], json!(2));
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["kind"], json!("retrieved"));
    assert_eq!(lines[1]["kind"], json!("read"));
}

#[test]
fn applied_requires_an_external_anchor_in_the_evidence_file() {
    let env = support::TestEnv::new();
    let (config, events) = enabled_config(&env, "applied");
    let id = hit('c');
    let evidence = env.write("applied-evidence/plan.md", format!("# plan\nhit {id}\n").as_bytes());

    let applied = json!({
        "kind": "applied",
        "hit_id": id,
        "ts": now(),
        "task_id": "task-1",
        "evidence": [evidence.to_string_lossy()]
    });
    let (code, response) = record(&config, &applied);
    assert_eq!(code, 0, "{response}");
    assert_eq!(response["wrote"], json!(true));

    // No evidence at all: rejected at the schema level.
    let mut missing = applied.clone();
    missing.as_object_mut().unwrap().remove("evidence");
    let (code, response) = record(&config, &missing);
    assert_eq!(code, 2);
    assert_eq!(response["code"], json!("INVALID_REQUEST"));
    assert_eq!(response["error"]["reason"], json!("required_for_applied"));

    // Evidence path does not exist.
    let mut absent = applied.clone();
    absent["evidence"] = json!([env.path().join("missing.md").to_string_lossy()]);
    let (code, response) = record(&config, &absent);
    assert_eq!(code, 2);
    assert_eq!(response["code"], json!("EVIDENCE_UNVERIFIED"));
    assert_eq!(response["error"]["reason"], json!("evidence_missing"));
    assert_eq!(response["wrote"], json!(false));

    // Evidence exists but does not contain the hit_id.
    let other = env.write("applied-evidence/other.md", b"no matching identifier\n");
    let mut unrelated = applied.clone();
    unrelated["evidence"] = json!([other.to_string_lossy()]);
    let (code, response) = record(&config, &unrelated);
    assert_eq!(code, 2);
    assert_eq!(response["code"], json!("EVIDENCE_UNVERIFIED"));
    assert_eq!(
        response["error"]["reason"],
        json!("evidence_without_hit_id")
    );

    let (code, stats) = stats(&config, Some(&id));
    assert_eq!(code, 0);
    assert_eq!(find_hit(&stats, &id)["applied"], json!(1));
    let size = std::fs::metadata(&events).unwrap().len();
    assert!(size > 0);
}

#[test]
fn confirmed_requires_actor_and_verifiable_evidence() {
    let env = support::TestEnv::new();
    let (config, _) = enabled_config(&env, "confirmed");
    let id = hit('d');
    let evidence = env.write(
        "confirmed-evidence/review.json",
        format!("{{\"verified\":\"{id}\"}}").as_bytes(),
    );

    let confirmed = json!({
        "kind": "confirmed",
        "hit_id": id,
        "ts": now(),
        "actor": "verifier",
        "evidence": [evidence.to_string_lossy()]
    });
    let (code, response) = record(&config, &confirmed);
    assert_eq!(code, 0, "{response}");
    assert_eq!(response["wrote"], json!(true));

    let mut unanchored = confirmed.clone();
    unanchored["evidence"] = json!([env.write("confirmed-evidence/none.md", b"nothing").to_string_lossy()]);
    let (code, response) = record(&config, &unanchored);
    assert_eq!(code, 2);
    assert_eq!(response["code"], json!("EVIDENCE_UNVERIFIED"));
    assert_eq!(
        response["error"]["reason"],
        json!("evidence_without_hit_id")
    );

    let mut anonymous = confirmed.clone();
    anonymous.as_object_mut().unwrap().remove("actor");
    let (code, response) = record(&config, &anonymous);
    assert_eq!(code, 2);
    assert_eq!(response["code"], json!("INVALID_REQUEST"));
    assert_eq!(response["error"]["reason"], json!("required_for_confirmed"));

    let (code, stats) = stats(&config, Some(&id));
    assert_eq!(code, 0);
    assert_eq!(find_hit(&stats, &id)["confirmed"], json!(1));
    assert_eq!(stats["totals"]["confirmed"], json!(1));
}

#[test]
fn duplicate_submissions_are_idempotent_and_counts_do_not_move() {
    let env = support::TestEnv::new();
    let (config, _) = enabled_config(&env, "idempotent");
    let id = hit('e');
    let event = retrieved(&id);

    let (code, first) = record(&config, &event);
    assert_eq!(code, 0);
    assert_eq!(first["wrote"], json!(true));
    let (code, second) = record(&config, &event);
    assert_eq!(code, 0, "a duplicate is a success");
    assert_eq!(second["code"], json!("EVENTS_DUPLICATE"));
    assert_eq!(second["wrote"], json!(false));
    assert_eq!(second["duplicate"], json!(true));
    assert_eq!(second["idempotency_key"], first["idempotency_key"]);

    let mut explicit = read_event(&id);
    explicit["idempotency_key"] = json!("caller-provided-key");
    let (code, _) = record(&config, &explicit);
    assert_eq!(code, 0);
    let (code, duplicate) = record(&config, &explicit);
    assert_eq!(code, 0);
    assert_eq!(duplicate["wrote"], json!(false));

    let (code, stats) = stats(&config, Some(&id));
    assert_eq!(code, 0);
    assert_eq!(find_hit(&stats, &id)["retrieved"], json!(1));
    assert_eq!(find_hit(&stats, &id)["read"], json!(1));
    let (_, lines, _) = export(&config, false);
    assert_eq!(lines.len(), 2);
}

#[test]
fn stats_and_export_agree_per_hit() {
    let env = support::TestEnv::new();
    let (config, _) = enabled_config(&env, "stats-export");
    let first = hit('f');
    let second = hit('1');
    let evidence = env.write("stats-evidence/commit.md", format!("commit {first}").as_bytes());

    let mut applied = json!({
        "kind": "applied",
        "hit_id": first,
        "ts": now(),
        "task_id": "t-2",
        "evidence": [evidence.to_string_lossy()]
    });
    applied["ts"] = json!(offset_days(-1));
    let mut confirmed = json!({
        "kind": "confirmed",
        "hit_id": first,
        "ts": now(),
        "actor": "user",
        "evidence": [evidence.to_string_lossy()]
    });
    confirmed["ts"] = json!(offset_days(-2));

    let recent_ts = now();
    for event in [
        json!({"kind": "retrieved", "hit_id": first, "ts": recent_ts}),
        read_event(&first),
        applied,
        confirmed,
        retrieved(&second),
        json!({"kind": "retrieved", "hit_id": second, "ts": offset_days(-3)}),
    ] {
        let (code, response) = record(&config, &event);
        assert_eq!(code, 0, "{event} -> {response}");
    }

    let (code, report) = stats(&config, None);
    assert_eq!(code, 0);
    assert_eq!(report["totals"]["hits"], json!(2));
    assert_eq!(report["totals"]["events"], json!(6));
    assert_eq!(report["totals"]["retrieved"], json!(3));
    assert_eq!(report["totals"]["read"], json!(1));
    assert_eq!(report["totals"]["applied"], json!(1));
    assert_eq!(report["totals"]["confirmed"], json!(1));

    let first_row = find_hit(&report, &first);
    assert_eq!(find_hit(&report, &second)["retrieved"], json!(2));
    assert_eq!(first_row["applied"], json!(1));
    assert_eq!(first_row["last_event_at"], json!(recent_ts));

    let (code, lines, summary) = export(&config, false);
    assert_eq!(code, 0);
    assert_eq!(summary["exported"], json!(lines.len() as u64));
    let mut from_export: std::collections::BTreeMap<(String, String), i64> =
        std::collections::BTreeMap::new();
    for line in &lines {
        let key = (
            line["hit_id"].as_str().unwrap().to_string(),
            line["kind"].as_str().unwrap().to_string(),
        );
        *from_export.entry(key).or_insert(0) += 1;
    }
    for row in report["hits"].as_array().unwrap() {
        let id = row["hit_id"].as_str().unwrap();
        for kind in ["retrieved", "read", "applied", "confirmed"] {
            let exported = from_export
                .get(&(id.to_string(), kind.to_string()))
                .copied()
                .unwrap_or(0);
            assert_eq!(
                row[kind],
                json!(exported),
                "{id}/{kind} differs between stats and export"
            );
        }
    }

    let (code, filtered) = stats(&config, Some(&second));
    assert_eq!(code, 0);
    assert_eq!(filtered["hits"].as_array().unwrap().len(), 1);
    assert_eq!(find_hit(&filtered, &second)["retrieved"], json!(2));
}

#[test]
fn export_never_contains_query_text_by_default() {
    let env = support::TestEnv::new();
    let (config, _) = enabled_config(&env, "query-default");
    let id = hit('2');
    let secret = "whitelisted-query-needle";
    let (code, response) = record(
        &config,
        &json!({
            "kind": "retrieved",
            "hit_id": id,
            "ts": now(),
            "query": secret,
            "expires_at": offset_days(1)
        }),
    );
    assert_eq!(code, 0, "{response}");

    let (code, lines, summary) = export(&config, false);
    assert_eq!(code, 0);
    assert_eq!(summary["exported"], json!(1));
    assert!(lines[0].get("query").is_none(), "default export leaked a query key");
    assert!(!lines[0].to_string().contains(secret));
    assert!(
        !run(&["events", "export", "--config", config.to_str().unwrap()])
            .stdout
            .contains(secret)
    );
}

#[test]
fn include_query_exports_only_unexpired_bodies() {
    let env = support::TestEnv::new();
    let (config, _) = enabled_config(&env, "query-include");
    let fresh = hit('3');
    let stale = hit('4');
    let fresh_text = "fresh-whitelisted-query";
    let stale_text = "stale-whitelisted-query";
    let (code, _) = record(
        &config,
        &json!({
            "kind": "retrieved",
            "hit_id": fresh,
            "ts": now(),
            "query": fresh_text,
            "expires_at": offset_days(1)
        }),
    );
    assert_eq!(code, 0);
    let (code, _) = record(
        &config,
        &json!({
            "kind": "read",
            "hit_id": stale,
            "ts": now(),
            "query": stale_text,
            "expires_at": offset_days(-1)
        }),
    );
    assert_eq!(code, 0);

    let (code, plain, _) = export(&config, false);
    assert_eq!(code, 0);
    assert_eq!(plain.len(), 2);
    assert!(plain.iter().all(|line| line.get("query").is_none()));

    let (code, lines, _) = export(&config, true);
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 2);
    let fresh_line = lines.iter().find(|line| line["hit_id"] == json!(fresh)).unwrap();
    let stale_line = lines.iter().find(|line| line["hit_id"] == json!(stale)).unwrap();
    assert_eq!(fresh_line["query"], json!(fresh_text));
    assert!(stale_line["query"].is_null(), "expired body must not be exported");
}

#[test]
fn prune_blanks_expired_query_text_and_keeps_counts() {
    let env = support::TestEnv::new();
    let (config, _) = enabled_config(&env, "prune-query");
    let id = hit('5');
    let secret = "expired-query-needle";
    let (code, _) = record(
        &config,
        &json!({
            "kind": "retrieved",
            "hit_id": id,
            "ts": now(),
            "query": secret,
            "expires_at": offset_days(-1)
        }),
    );
    assert_eq!(code, 0);
    let (code, _) = record(&config, &read_event(&id));
    assert_eq!(code, 0);

    let (_, before) = stats(&config, Some(&id));
    let (code, response) = prune(&config);
    assert_eq!(code, 0, "{response}");
    assert_eq!(response["code"], json!("EVENTS_PRUNED"));
    assert_eq!(response["pruned_query_text"], json!(1));
    assert_eq!(response["pruned_events"], json!(0));
    assert_eq!(response["remaining_events"], json!(2));
    assert_eq!(response["preserved_count_rows"], json!(2));
    assert_eq!(response["wrote"], json!(true));

    let (_, after) = stats(&config, Some(&id));
    assert_eq!(before["hits"], after["hits"]);
    assert_eq!(find_hit(&after, &id)["retrieved"], json!(1));
    assert_eq!(find_hit(&after, &id)["read"], json!(1));

    let (code, lines, _) = export(&config, true);
    assert_eq!(code, 0);
    let row = lines.iter().find(|line| line["kind"] == json!("retrieved")).unwrap();
    assert!(row["query"].is_null());
    assert!(!serde_json::to_string(&lines).unwrap().contains(secret));
}

#[test]
fn retention_deletes_old_details_but_keeps_aggregate_counts() {
    let env = support::TestEnv::new();
    let (config, _) = config(
        &env,
        "retention",
        json!({"enabled": true, "retention_days": 1}),
        None,
    );
    let old = hit('6');
    let recent = hit('7');
    let (code, response) = record(
        &config,
        &json!({"kind": "retrieved", "hit_id": old, "ts": "2020-01-01T00:00:00Z"}),
    );
    assert_eq!(code, 0, "{response}");
    let (code, _) = record(&config, &retrieved(&recent));
    assert_eq!(code, 0);

    let (code, response) = prune(&config);
    assert_eq!(code, 0, "{response}");
    assert_eq!(response["pruned_events"], json!(1));
    assert_eq!(response["pruned_query_text"], json!(0));
    assert_eq!(response["remaining_events"], json!(1));
    assert_eq!(response["preserved_count_rows"], json!(2));

    let (code, stats) = stats(&config, None);
    assert_eq!(code, 0);
    assert_eq!(stats["totals"]["hits"], json!(2), "counters survive detail deletion");
    assert_eq!(find_hit(&stats, &old)["retrieved"], json!(1));
    assert_eq!(find_hit(&stats, &recent)["retrieved"], json!(1));

    let (code, lines, _) = export(&config, false);
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 1, "only the recent detail survives");
    assert_eq!(lines[0]["hit_id"], json!(recent));

    let (code, response) = prune(&config);
    assert_eq!(code, 0);
    assert_eq!(response["pruned_events"], json!(0));
    assert_eq!(response["wrote"], json!(false));
}

#[test]
fn unwritable_and_corrupt_event_storage_fail_non_zero() {
    let env = support::TestEnv::new();
    let blocker = env.write("blocker", b"a regular file, not a directory");
    let blocked = blocker.join("state").join("retrieval_events.sqlite");
    let (config, _) = config(
        &env,
        "blocked",
        json!({"enabled": true, "path": blocked.to_string_lossy()}),
        Some(&blocked),
    );

    let (code, response) = record(&config, &retrieved(&hit('8')));
    assert_eq!(code, 11, "{response}");
    assert_eq!(response["ok"], json!(false));
    assert_eq!(response["wrote"], json!(false));
    assert_eq!(response["code"], json!("EVENTS_STORAGE_ERROR"));
    assert!(!blocked.exists());

    let (config, events) = enabled_config(&env, "corrupt");
    std::fs::create_dir_all(events.parent().unwrap()).unwrap();
    std::fs::write(&events, b"this is not a sqlite database\n".repeat(16)).unwrap();

    let (code, response) = record(&config, &retrieved(&hit('9')));
    assert_eq!(code, 11, "{response}");
    assert_eq!(response["ok"], json!(false));
    assert_eq!(response["code"], json!("EVENTS_STORAGE_ERROR"));

    let (code, response) = stats(&config, None);
    assert_eq!(code, 11);
    assert_eq!(response["code"], json!("EVENTS_STORAGE_ERROR"));

    let (code, _, summary) = export(&config, false);
    assert_eq!(code, 11);
    assert_eq!(summary["code"], json!("EVENTS_STORAGE_ERROR"));

    let (code, response) = prune(&config);
    assert_eq!(code, 11);
    assert_eq!(response["code"], json!("EVENTS_STORAGE_ERROR"));
}

#[test]
fn telemetry_disabled_never_creates_event_storage() {
    let env = support::TestEnv::new();
    let events = env
        .path()
        .join("disabled-state")
        .join("retrieval_events.sqlite");
    let (config, _) = config(
        &env,
        "disabled",
        json!({"enabled": false, "path": events.to_string_lossy()}),
        Some(&events),
    );
    assert!(!events.exists());

    let (code, response) = record(&config, &retrieved(&hit('a')));
    assert_eq!(code, 11);
    assert_eq!(response["code"], json!("TELEMETRY_DISABLED"));
    assert_eq!(response["wrote"], json!(false));

    let (code, response) = prune(&config);
    assert_eq!(code, 11);
    assert_eq!(response["code"], json!("TELEMETRY_DISABLED"));

    let (code, response) = stats(&config, None);
    assert_eq!(code, 11);
    assert_eq!(response["code"], json!("TELEMETRY_DISABLED"));

    let (code, lines, summary) = export(&config, true);
    assert_eq!(code, 11);
    assert!(lines.is_empty());
    assert_eq!(summary["code"], json!("TELEMETRY_DISABLED"));

    assert!(!events.exists());
    assert!(
        !events.parent().unwrap().exists(),
        "a disabled store must not create its directory"
    );
}

#[test]
fn replay_ignores_the_event_store_and_leaves_it_untouched() {
    let env = support::TestEnv::new();
    let root = env.root("replay-root");
    let archive = agent_memory::archive::Archive::init(&root).unwrap();
    let source = env.path().join("codex-rollout.jsonl");
    let mut text = String::new();
    text.push_str(&support::codex_meta("s-p2"));
    text.push('\n');
    text.push_str(&support::codex_user("needlealpha telemetry isolation"));
    text.push('\n');
    std::fs::write(&source, text).unwrap();
    let report = agent_memory::capture::capture_source(&archive, &source, "codex", "default", None)
        .expect("capture");
    assert_eq!(report.status, "captured", "{report:?}");

    let events = root.join("state").join("retrieval_events.sqlite");
    let (config, _) = config(
        &env,
        "replay-telemetry",
        json!({"enabled": true, "path": events.to_string_lossy()}),
        Some(&events),
    );
    for event in [retrieved(&hit('a')), read_event(&hit('a'))] {
        let (code, response) = record(&config, &event);
        assert_eq!(code, 0, "{response}");
    }
    let before = std::fs::read(&events).unwrap();
    assert!(before.len() > 0);

    let replay = run(&[
        "replay",
        "--root",
        root.to_str().unwrap(),
        "--domain",
        "default",
    ]);
    assert_eq!(replay.code, 0, "{}", replay.stderr);
    assert_eq!(
        std::fs::read(&events).unwrap(),
        before,
        "replay must not touch the event store"
    );
    assert!(!replay.stdout.contains("retrieval_events"));

    // No derived artifact may be the event store or mirror it.
    let derived = root.join("derived");
    let mut stack = vec![derived];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                assert!(
                    !path.to_string_lossy().contains("retrieval_events"),
                    "event store leaked into {}",
                    path.display()
                );
            }
        }
    }

    // A corrupted store still does not affect replay: it is not an input.
    std::fs::write(&events, b"corrupted on purpose").unwrap();
    let replay = run(&[
        "replay",
        "--root",
        root.to_str().unwrap(),
        "--domain",
        "default",
    ]);
    assert_eq!(replay.code, 0, "{}", replay.stderr);
    assert_eq!(std::fs::read(&events).unwrap(), b"corrupted on purpose");
}

#[test]
fn replay_report_coverage_and_scrub_never_reference_the_event_store() {
    // Reverse tripwire: the event store is not an input of any evidence-layer
    // command. Only `src/recall/telemetry.rs` and the CLI may name it.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for rel in [
        "replay.rs",
        "report.rs",
        "coverage.rs",
        "scrub.rs",
        "capture.rs",
        "mcp.rs",
        "recall/search.rs",
        "recall/read.rs",
        "recall/ops.rs",
    ] {
        let source = std::fs::read_to_string(root.join(rel)).expect("source");
        assert!(
            !source.contains("retrieval_events"),
            "{rel} must not reference the event store"
        );
        assert!(
            !source.contains("recall::telemetry"),
            "{rel} must not import the telemetry module"
        );
    }
}

// ---------------------------------------------------------------------------
// Configuration (`telemetry.retention_days` and store isolation)
// ---------------------------------------------------------------------------

fn v2_value() -> Value {
    json!({
        "version": 2,
        "sources": [
            {"source_id": "kb", "kind": "kb", "root": "C:/vault", "enabled": true}
        ],
        "source_order": ["kb"],
        "interpreter": {"kb": {"command": "C:/python.exe"}},
        "domain": "default",
        "telemetry": {"enabled": false}
    })
}

fn detail_path(error: &agent_memory::recall::contract::RecallError) -> String {
    let detail = error.detail.clone().unwrap_or(Value::Null);
    detail["path"].as_str().unwrap_or_default().to_string()
}

fn detail_reason(error: &agent_memory::recall::contract::RecallError) -> String {
    let detail = error.detail.clone().unwrap_or(Value::Null);
    detail["reason"].as_str().unwrap_or_default().to_string()
}

#[test]
fn v2_telemetry_accepts_retention_days_and_rejects_out_of_range_or_unknown_keys() {
    use agent_memory::recall::config::RecallConfig;

    let mut value = v2_value();
    value["telemetry"] = json!({
        "enabled": true,
        "path": "C:/state/retrieval_events.sqlite"
    });
    let config = RecallConfig::from_value(&value).expect("default retention");
    let telemetry = config.telemetry.unwrap();
    assert_eq!(telemetry.retention_days, 365);

    for (days, accepted) in [(1u64, true), (3650, true), (0, false), (3651, false)] {
        let mut value = v2_value();
        value["telemetry"] = json!({
            "enabled": true,
            "path": "C:/state/retrieval_events.sqlite",
            "retention_days": days
        });
        match RecallConfig::from_value(&value) {
            Ok(config) => {
                assert!(accepted, "retention_days={days} must be rejected");
                assert_eq!(config.telemetry.unwrap().retention_days, days as u32);
            }
            Err(error) => {
                assert!(!accepted, "retention_days={days} must be accepted");
                assert_eq!(detail_path(&error), "telemetry.retention_days");
                assert_eq!(detail_reason(&error), "out_of_range");
            }
        }
    }

    for bad in [json!("7"), json!(true), json!(1.5)] {
        let mut value = v2_value();
        value["telemetry"] = json!({
            "enabled": true,
            "path": "C:/state/retrieval_events.sqlite",
            "retention_days": bad
        });
        let error = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(detail_path(&error), "telemetry.retention_days");
        assert_eq!(detail_reason(&error), "invalid_type");
    }

    let mut value = v2_value();
    value["telemetry"] = json!({
        "enabled": false,
        "retention_days_typo": 7
    });
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(
        detail_path(&error),
        "telemetry.retention_days_typo",
        "unknown telemetry keys are still rejected"
    );
    assert_eq!(detail_reason(&error), "unknown_key");

    let mut v1 = v2_value();
    v1["version"] = json!(1);
    v1.as_object_mut().unwrap().remove("domain");
    v1["telemetry"] = json!({"enabled": false, "retention_days": 7});
    let error = RecallConfig::from_value(&v1).unwrap_err();
    assert_eq!(detail_path(&error), "telemetry");
}

#[test]
fn event_storage_may_not_live_inside_archive_objects_or_derived() {
    use agent_memory::recall::config::RecallConfig;

    let mut value = v2_value();
    value["sources"] = json!([
        {"source_id": "ar", "kind": "archive", "root": "C:/archive", "enabled": true}
    ]);
    value["source_order"] = json!(["ar"]);
    for reserved in [
        "C:/archive/archive/objects/aa/bb/events.sqlite",
        "C:/archive/derived/generations/g1/events.sqlite",
    ] {
        value["telemetry"] = json!({"enabled": true, "path": reserved});
        let error = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(detail_path(&error), "telemetry.path");
        assert_eq!(detail_reason(&error), "reserved_area");
    }

    value["telemetry"] = json!({
        "enabled": true,
        "path": "C:/archive/state/retrieval_events.sqlite"
    });
    RecallConfig::from_value(&value).expect("state/ is the documented location");
}

#[test]
fn query_text_cannot_outlive_the_configured_ttl() {
    let env = support::TestEnv::new();
    let (config, events) = enabled_config(&env, "query-ttl");
    let ts = now();
    let within = offset_days(7);
    let (code, response) = record(
        &config,
        &json!({"kind": "retrieved", "hit_id": hit('a'), "ts": ts, "query": "q", "expires_at": within}),
    );
    assert_eq!(code, 0, "{response}");

    for (expires_at, reason) in [
        (offset_days(8), "exceeds_query_ttl"),
        ("9999-12-31T23:59:59Z".to_string(), "exceeds_query_ttl"),
        ("2026-02-30T00:00:00Z".to_string(), "invalid_timestamp"),
    ] {
        let (code, response) = record(
            &config,
            &json!({"kind": "retrieved", "hit_id": hit('b'), "ts": now(), "query": "q", "expires_at": expires_at}),
        );
        assert_eq!(code, 2, "{response}");
        assert_eq!(response["wrote"], json!(false));
        assert_eq!(response["error"]["reason"], json!(reason));
    }
    let (_, stats) = stats(&config, None);
    assert_eq!(stats["totals"]["events"], json!(1), "{stats}");
    assert!(events.exists());
}

#[test]
fn prune_never_initialises_schema_in_a_foreign_database() {
    let env = support::TestEnv::new();
    let (config, events) = enabled_config(&env, "foreign-db");
    std::fs::create_dir_all(events.parent().unwrap()).unwrap();
    {
        let conn = rusqlite::Connection::open(&events).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER); INSERT INTO other VALUES (1);")
            .unwrap();
    }
    let (code, response) = prune(&config);
    assert_eq!(code, 11, "{response}");
    assert_eq!(response["code"], json!("EVENTS_STORAGE_ERROR"));
    let conn = rusqlite::Connection::open(&events).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name <> 'other'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "prune created event tables in a foreign database");
}
