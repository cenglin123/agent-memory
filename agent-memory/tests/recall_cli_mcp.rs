//! P3: the CLI and the stdio MCP wrapper share one core, so the same request
//! must produce the same envelope, `code` and `isError`; the MCP surface must
//! expose exactly the documented read-only tools and must never block the CLI.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

mod support;

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

fn kb_config(env: &support::TestEnv, name: &str) -> (PathBuf, PathBuf) {
    let root = env.root(name);
    let script = root.join(".meta").join("scripts").join("ask.py");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall/kb_stub/ask.py"),
        &script,
    )
    .expect("install kb stub");
    env.write(&format!("{name}/notes/alpha.md"), CHUNK.as_bytes());
    let config = env.write_json(
        &format!("{name}-config.json"),
        &json!({
            "version": 1,
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
            }
        }),
    );
    (root, config)
}

struct CliRun {
    code: i32,
    value: Value,
}

fn run_cli(command: &str, config: &Path, request: &Value) -> CliRun {
    run_cli_flags(command, config, request, &[])
}

fn run_cli_flags(command: &str, config: &Path, request: &Value, extra: &[&str]) -> CliRun {
    let output = Command::new(bin())
        .arg(command)
        .arg("--config")
        .arg(config)
        .args(extra)
        .arg("--request")
        .arg(serde_json::to_string(request).unwrap())
        .output()
        .expect("run agent-memory");
    let value = serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    CliRun {
        code: output.status.code().expect("exit code"),
        value,
    }
}

struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
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
        session.notify("notifications/initialized", json!({}));
        session
    }

    fn send(&mut self, value: Value) {
        writeln!(self.stdin, "{value}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn read_response(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read response");
        assert!(read > 0, "mcp closed its stdout");
        serde_json::from_str(&line).expect("response is JSON")
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let response = self.read_response();
        assert_eq!(response["id"], json!(id));
        response
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

#[test]
fn cli_and_mcp_return_the_same_search_envelope() {
    let env = support::TestEnv::new();
    let (_root, config) = kb_config(&env, "eq-kb");
    let request = json!({"query": "stub", "limit": 5});

    let cli = run_cli("memory_search", &config, &request);
    assert_eq!(cli.code, 0);
    assert_eq!(cli.value["contract_version"], json!(2));
    assert_eq!(cli.value["results"].as_array().unwrap().len(), 1);

    let mut mcp = McpSession::start(&config);
    let response = mcp.call("memory_search", request.clone());
    assert!(response.get("error").is_none(), "unexpected error: {response}");
    let result = &response["result"];
    assert_eq!(result["isError"], json!(false));
    assert_eq!(
        result["structuredContent"], cli.value,
        "the same request must produce the same envelope"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), cli.value);
}

#[test]
fn non_default_domain_does_not_restrict_live_sources() {
    // P1: only Archive records participate in the access-label predicate. A
    // live KB source must keep answering under any effective domain and must
    // never be replaced by a DOMAIN_* refusal.
    let env = support::TestEnv::new();
    let (_root, config) = kb_config(&env, "domain-kb");

    let default_search = run_cli("memory_search", &config, &json!({"query": "stub"}));
    assert_eq!(default_search.code, 0);
    let reference = default_search.value["results"][0]["ref"].clone();

    let request = json!({"query": "stub", "domain": "work"});
    let cli = run_cli("memory_search", &config, &request);
    assert_eq!(cli.code, 0, "{}", cli.value);
    assert_eq!(cli.value["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        cli.value["results"][0]["hit_id"],
        default_search.value["results"][0]["hit_id"],
        "a live source hit is domain-independent"
    );

    let mut mcp = McpSession::start(&config);
    let response = mcp.call("memory_search", request.clone());
    assert_eq!(response["result"]["isError"], json!(false));
    assert_eq!(response["result"]["structuredContent"], cli.value);

    let read_request = json!({"ref": reference, "domain": "work"});
    let read = run_cli("memory_read", &config, &read_request);
    assert_eq!(read.code, 0, "{}", read.value);
    assert_eq!(read.value["ok"], json!(true));

    let response = mcp.call("memory_read", read_request);
    assert_eq!(response["result"]["isError"], json!(false));
    assert_eq!(response["result"]["structuredContent"], read.value);
}

#[test]
fn cli_and_mcp_read_back_the_same_reference() {
    let env = support::TestEnv::new();
    let (_root, config) = kb_config(&env, "read-kb");
    let cli_search = run_cli("memory_search", &config, &json!({"query": "stub"}));
    let reference = cli_search.value["results"][0]["ref"].clone();
    assert_eq!(reference["schema_gen"], json!("md-v1"));
    let request = json!({"ref": reference, "context_before": 0, "context_after": 0});

    let cli = run_cli("memory_read", &config, &request);
    assert_eq!(cli.code, 0);
    assert_eq!(cli.value["ok"], json!(true));
    assert_eq!(cli.value["text"], json!(CHUNK));

    let mut mcp = McpSession::start(&config);
    let response = mcp.call("memory_read", request);
    assert_eq!(response["result"]["isError"], json!(false));
    assert_eq!(response["result"]["structuredContent"], cli.value);
}

#[test]
fn invalid_requests_map_to_exit_2_and_jsonrpc_minus_32602() {
    let env = support::TestEnv::new();
    let (_root, config) = kb_config(&env, "invalid-kb");
    let request = json!({"query": "stub", "offset": 10_001});

    let cli = run_cli("memory_search", &config, &request);
    assert_eq!(cli.code, 2);
    assert_eq!(cli.value["errors"][0]["code"], json!("INVALID_OFFSET"));

    let mut mcp = McpSession::start(&config);
    let response = mcp.call("memory_search", request);
    let error = response.get("error").expect("protocol error");
    assert_eq!(error["code"], json!(-32602));
    assert_eq!(error["data"]["isError"], json!(true));
    assert_eq!(error["data"]["code"], json!("INVALID_OFFSET"));
    assert_eq!(error["data"]["response"], cli.value);

    // A read failure is an `isError` result, not a protocol error.
    let missing = json!({"ref": {
        "source_id": "kb", "source_kind": "kb",
        "root_sha256": "00", "rel_path": "notes/alpha.md", "schema_gen": "md-v1",
        "locator": {}, "fingerprint": {"range_sha256": "00"},
        "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": 3}
    }});
    let cli = run_cli("memory_read", &config, &missing);
    assert_eq!(cli.code, 5);
    assert_eq!(cli.value["code"], json!("REF_ROOT_MISMATCH"));
    let mut mcp = McpSession::start(&config);
    let response = mcp.call("memory_read", missing);
    assert_eq!(response["result"]["isError"], json!(true));
    assert_eq!(response["result"]["structuredContent"], cli.value);
}

#[test]
fn strict_snapshot_flag_runs_the_kb_child_inside_the_copy() {
    let env = support::TestEnv::new();
    let (root, config) = kb_config(&env, "strict-kb");
    let strict = run_cli_flags(
        "memory_search",
        &config,
        &json!({"query": "stub"}),
        &["--strict-snapshot"],
    );
    assert_eq!(strict.code, 0, "{:?}", strict.value);
    assert_eq!(strict.value["results"].as_array().unwrap().len(), 1);
    // The ref keeps the configured root hash, so it reads back from the real root.
    let normal = run_cli("memory_search", &config, &json!({"query": "stub"}));
    assert_eq!(
        strict.value["results"][0]["ref"]["root_sha256"],
        normal.value["results"][0]["ref"]["root_sha256"]
    );
    let reference = strict.value["results"][0]["ref"].clone();
    let read = run_cli("memory_read", &config, &json!({"ref": reference}));
    assert_eq!(read.code, 0);
    assert_eq!(read.value["text"], json!(CHUNK));
    // The original root was not rewritten by the strict run.
    assert!(root.join("notes/alpha.md").is_file());
}

#[test]
fn strict_snapshot_flag_serves_an_opencode_source_through_the_cli() {
    // Mirrors the verifier's reproduction: the same config must work normally
    // and under `--strict-snapshot` for a SQLite source, search and read.
    let env = support::TestEnv::new();
    let fixture = support::build_v1_hits(&env, "cli-strict-oc", 2, 2);
    let config = env.write_json(
        "cli-strict-oc.json",
        &env.config_value(
            vec![env.opencode_source("oc", &fixture.root)],
            vec!["oc"],
        ),
    );

    let normal = run_cli("memory_search", &config, &json!({"query": support::NEEDLE}));
    assert_eq!(normal.code, 0, "{:?}", normal.value);
    let strict = run_cli_flags(
        "memory_search",
        &config,
        &json!({"query": support::NEEDLE}),
        &["--strict-snapshot"],
    );
    assert_eq!(strict.code, 0, "{:?}", strict.value);
    assert_eq!(strict.value["source_status"][0]["status"], json!("ok"));
    assert_eq!(
        strict.value["results"].as_array().unwrap().len(),
        normal.value["results"].as_array().unwrap().len()
    );

    let reference = strict.value["results"][0]["ref"].clone();
    let read = run_cli_flags(
        "memory_read",
        &config,
        &json!({"ref": reference}),
        &["--strict-snapshot"],
    );
    assert_eq!(read.code, 0, "{:?}", read.value);
    assert_eq!(read.value["ok"], json!(true));
    assert!(read.value["text"].as_str().unwrap().contains(support::NEEDLE));
}

#[test]
fn mcp_exposes_exactly_four_read_only_tools() {
    let env = support::TestEnv::new();
    let (_root, config) = kb_config(&env, "tools-kb");
    let mut mcp = McpSession::start(&config);
    let response = mcp.request("tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    let names: Vec<&str> = tools.iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "memory_search",
            "memory_read",
            "memory_recent",
            "memory_qualified"
        ]
    );
    let description = tools[0]["description"].as_str().unwrap();
    assert!(description.contains("Codex"));
    assert!(description.contains("session_id"));
    assert!(description.contains("no Archive required"));
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let expected = if name == "memory_read" { json!(false) } else { json!(true) };
        assert_eq!(tool["annotations"]["readOnlyHint"], expected, "{name}");
        for banned in ["write", "delete", "capture", "init", "update"] {
            assert!(!name.contains(banned), "no write tool: {name}");
        }
    }
    let unknown = mcp.call("memory_write", json!({}));
    assert_eq!(unknown["error"]["code"], json!(-32601));
}

#[test]
fn a_running_mcp_server_does_not_block_the_cli() {
    let env = support::TestEnv::new();
    let (_root, config) = kb_config(&env, "nonblock-kb");
    let mut mcp = McpSession::start(&config);
    // Keep the server alive and busy-ish: it must not hold a lock the CLI needs.
    let _ = mcp.request("tools/list", json!({}));

    let config_for_thread = config.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = run_cli("memory_search", &config_for_thread, &json!({"query": "stub"}));
        let _ = sender.send(result.code);
    });
    let code = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("CLI must not be blocked by the MCP server");
    assert_eq!(code, 0);

    // And the server still answers afterwards.
    let response = mcp.request("tools/list", json!({}));
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 4);
}

#[test]
fn mcp_search_schema_declares_the_session_id_filter() {
    let env = support::TestEnv::new();
    let (_root, config) = kb_config(&env, "session-schema-kb");
    let mut mcp = McpSession::start(&config);
    let response = mcp.request("tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().unwrap();
    let search = tools
        .iter()
        .find(|tool| tool["name"] == json!("memory_search"))
        .expect("memory_search");
    let session = &search["inputSchema"]["properties"]["session_id"];
    assert_eq!(session["type"], json!("string"));
    assert_eq!(session["minLength"], json!(1));
    assert_eq!(session["maxLength"], json!(200));
    let domain = &search["inputSchema"]["properties"]["domain"];
    assert_eq!(domain["type"], json!("string"));
    assert_eq!(domain["minLength"], json!(1));
    assert_eq!(domain["maxLength"], json!(64));
    assert_eq!(
        search["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        17,
        "v2 adds domain next to session_id; P4 adds related_hops; \
         full-text L3 adds mode/since/until/roles/kinds/session_kinds/order/group_by_session"
    );
    // The indexed-mode surface is declared in the same schema as the CLI
    // contract (L5b adds the hybrid and legacy modes).
    let mode = &search["inputSchema"]["properties"]["mode"];
    assert_eq!(mode["enum"], json!(["auto", "exact", "hybrid", "legacy"]));
    assert!(search["inputSchema"]["properties"]["since"].is_object());
    assert!(search["inputSchema"]["properties"]["group_by_session"].is_object());
    assert_eq!(
        search["inputSchema"]["properties"]["session_kinds"]["items"]["enum"],
        json!(["interactive", "worker", "unknown"])
    );

    let read = tools
        .iter()
        .find(|tool| tool["name"] == json!("memory_read"))
        .expect("memory_read");
    // The optional read-side session id only keys the local device reuse
    // counter; it never filters, authorizes or changes the returned bytes.
    let read_session = read["inputSchema"]["properties"]
        .get("session_id")
        .expect("optional reuse session id");
    assert_eq!(read_session["minLength"], json!(1));
    assert!(read_session["description"]
        .as_str()
        .unwrap()
        .contains("reuse"));
}
