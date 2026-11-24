//! Guard for the suite skill `assets/skills/agent-memory/` (agent-memory skill entry).
//!
//! The skill under `../assets/skills/agent-memory` is the single source of the
//! `agent-memory` skill. This test keeps the documentation honest against the
//! binary it documents:
//!
//! * frontmatter parses and `name` equals the directory name;
//! * the field table under each `<!-- am-fields: <op> -->` marker equals the
//!   `inputSchema.properties` of the matching MCP tool (the same schema the CLI
//!   validates against);
//! * every ```` ```json am-example:<op> ```` request really runs with
//!   `--request @file` on a synthetic recall.json fixture (exit 0, JSON stdout,
//!   empty `errors[]`), and every `am-example:learning` argv runs against a
//!   temporary learning root;
//! * no write/maintenance command appears as an executable example;
//! * files are UTF-8 without BOM, LF, and carry no real user path.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod support;

const SESSION: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_SESSION: &str = "22222222-2222-4222-8222-222222222222";
/// Learning subcommands the skill is allowed to run (read + task lifecycle +
/// qualification).
const LEARNING_EXPECTED: [&str; 8] = [
    "workspace",
    "inject",
    "create",
    "claim",
    "show",
    "abandon",
    "qualify",
    "qualified",
];
/// Write/maintenance commands that must never appear as an executable example.
const FORBIDDEN_EXAMPLES: [&str; 15] = [
    "capture",
    "capture-all",
    "index",
    "compact",
    "scrub",
    "replay",
    "ingest",
    "dedup",
    "validate",
    "gate",
    "reuse",
    "migrate",
    "init",
    "audit-grounding",
    "eval-recall",
];

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
}

fn skill_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/skills/agent-memory")
}

fn skill_files() -> Vec<PathBuf> {
    let dir = skill_dir();
    assert!(
        dir.is_dir(),
        "skill source directory is missing: {}",
        dir.display()
    );
    let mut files = vec![dir.join("SKILL.md")];
    let mut references: Vec<PathBuf> = std::fs::read_dir(dir.join("references"))
        .expect("references directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().map(|ext| ext == "md").unwrap_or(false))
        .collect();
    references.sort();
    files.extend(references);
    files
}

fn read_skill_file(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("read skill file");
    assert!(
        !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        "{} starts with a UTF-8 BOM",
        path.display()
    );
    assert!(
        !bytes.contains(&b'\r'),
        "{} contains CR (LF-only required)",
        path.display()
    );
    let text = String::from_utf8(bytes).expect("skill files are UTF-8");
    for needle in [
        "C:\\Users\\",
        "C:/Users/",
        "/home/",
        "/Users/",
        "<user-name>",
    ] {
        assert!(
            !text.contains(needle),
            "{} contains a real path fragment: {needle}",
            path.display()
        );
    }
    text
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

fn frontmatter(text: &str) -> Vec<(String, String)> {
    let mut lines = text.lines();
    assert_eq!(
        lines.next().map(str::trim),
        Some("---"),
        "frontmatter must open on line 1"
    );
    let mut fields = Vec::new();
    for line in lines {
        if line.trim() == "---" {
            return fields;
        }
        if let Some((key, value)) = line.split_once(':') {
            fields.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    panic!("frontmatter is not closed");
}

#[test]
fn frontmatter_name_is_the_directory_name() {
    let text = read_skill_file(&skill_dir().join("SKILL.md"));
    let fields = frontmatter(&text);
    let name = fields
        .iter()
        .find(|(key, _)| key == "name")
        .map(|(_, value)| value.clone())
        .expect("frontmatter name");
    assert_eq!(name, "agent-memory");
    assert_eq!(
        skill_dir().file_name().unwrap().to_string_lossy(),
        name,
        "frontmatter name must equal the skill directory name"
    );
    let description = fields
        .iter()
        .find(|(key, _)| key == "description")
        .map(|(_, value)| value.clone())
        .expect("frontmatter description");
    assert!(!description.is_empty(), "description must not be empty");
    for trigger in ["以前说过什么", "历史会话", "memory_search", "recall"] {
        assert!(
            description.contains(trigger),
            "description is missing the trigger {trigger}"
        );
    }
    assert!(
        text.lines().count() <= 150,
        "SKILL.md must stay within 150 lines, got {}",
        text.lines().count()
    );
}

// ---------------------------------------------------------------------------
// Field-set guard: references/requests.md vs MCP tools/list
// ---------------------------------------------------------------------------

/// Parses `<!-- am-fields: <op> -->` followed by a markdown table whose first
/// column is the field name. The header row and the `|---|---|` separator are
/// skipped; parsing stops at the first non-table line after the body.
fn marker_tables(text: &str) -> Vec<(String, Vec<String>)> {
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    let mut current: Option<(String, bool)> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("<!-- am-fields:") {
            let name = rest
                .trim_end_matches("-->")
                .trim()
                .trim_end_matches("-->")
                .trim()
                .to_string();
            current = Some((name, false));
            continue;
        }
        let Some((name, started)) = current.take() else {
            continue;
        };
        is_table_line(trimmed, &name, started, &mut tables, &mut current);
    }
    tables
}

fn is_table_line(
    trimmed: &str,
    name: &str,
    started: bool,
    tables: &mut Vec<(String, Vec<String>)>,
    current: &mut Option<(String, bool)>,
) {
    if !trimmed.starts_with('|') {
        // A blank line or prose after a started table closes it.
        return;
    }
    if !started {
        // Header row.
        *current = Some((name.to_string(), true));
        return;
    }
    let cells: Vec<&str> = trimmed
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect();
    if cells
        .iter()
        .all(|cell| cell.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '))
    {
        *current = Some((name.to_string(), true));
        return;
    }
    let field = cells[0].trim_matches('`').trim().to_string();
    if let Some(index) = tables.iter().position(|(existing, _)| existing == name) {
        tables[index].1.push(field);
    } else {
        tables.push((name.to_string(), vec![field]));
    }
    *current = Some((name.to_string(), true));
}

fn mcp_schemas(config: &Path) -> Value {
    let mut child = Command::new(bin())
        .arg("mcp")
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).expect("tools/list response");
    let _ = child.kill();
    let _ = child.wait();
    response["result"].clone()
}

fn code_field_sets(tools: &Value) -> Vec<(String, Vec<String>)> {
    let mut sets = Vec::new();
    for tool in tools["tools"].as_array().expect("tools array") {
        let name = tool["name"].as_str().expect("tool name").to_string();
        let mut fields: Vec<String> = tool["inputSchema"]["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect();
        fields.sort();
        sets.push((name.clone(), fields));
        if name == "memory_search" {
            let mut budget: Vec<String> = tool["inputSchema"]["properties"]["budget"]["properties"]
                .as_object()
                .expect("budget properties")
                .keys()
                .cloned()
                .collect();
            budget.sort();
            sets.push(("budget".to_string(), budget));
        }
    }
    sets
}

#[test]
fn documented_field_sets_match_the_accepted_request_fields() {
    let env = support::TestEnv::new();
    let fixture = build_fixture(&env);
    let documented = marker_tables(&read_skill_file(
        &skill_dir().join("references/requests.md"),
    ));
    let coded = code_field_sets(&mcp_schemas(&fixture.config));

    for (name, code_fields) in &coded {
        let (_, doc_fields) = documented
            .iter()
            .find(|(doc_name, _)| doc_name == name)
            .unwrap_or_else(|| {
                panic!(
                    "references/requests.md is missing the table marked am-fields: {name}; \
                     coded fields: {code_fields:?}"
                )
            });
        let mut doc_sorted = doc_fields.clone();
        doc_sorted.sort();
        assert_eq!(
            &doc_sorted, code_fields,
            "field set drift for {name}: document and contract differ"
        );
    }
    for (name, _) in &documented {
        assert!(
            coded.iter().any(|(code_name, _)| code_name == name),
            "references/requests.md documents an unknown operation: {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Example runner
// ---------------------------------------------------------------------------

struct Example {
    file: String,
    op: String,
    body: String,
}

fn examples_in(path: &Path, text: &str) -> Vec<Example> {
    let mut examples = Vec::new();
    let mut in_fence = false;
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if !in_fence {
                in_fence = true;
                if let Some(op) = rest.strip_prefix("json am-example:") {
                    current = Some((op.trim().to_string(), String::new()));
                }
            } else {
                in_fence = false;
                if let Some((op, body)) = current.take() {
                    examples.push(Example {
                        file: path.display().to_string(),
                        op,
                        body,
                    });
                }
            }
            continue;
        }
        if in_fence {
            if let Some((_, body)) = current.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    assert!(
        current.is_none(),
        "unterminated example fence in {}",
        path.display()
    );
    examples
}

struct Fixture {
    config: PathBuf,
}

fn claude_line(session: &str, uuid: &str, time: &str, kind: &str, content: Value) -> String {
    json!({
        "type": kind,
        "message": {"role": kind, "content": content},
        "timestamp": time,
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

fn build_fixture(env: &support::TestEnv) -> Fixture {
    let root = env.root("claude");
    let project = root.join("proj-alpha");
    std::fs::create_dir_all(&project).unwrap();
    let lines = vec![
        claude_line(
            SESSION,
            "u1",
            "2026-10-20T10:00:00Z",
            "user",
            json!("deployment needle alpha"),
        ),
        claude_line(
            SESSION,
            "a1",
            "2026-10-20T10:01:00Z",
            "assistant",
            json!([{"type": "text", "text": "answer needle beta"}]),
        ),
        claude_line(
            SESSION,
            "u2",
            "2026-10-20T10:02:00Z",
            "user",
            json!("needle again gamma"),
        ),
    ];
    env.write(
        &format!("claude/proj-alpha/{SESSION}.jsonl"),
        (lines.join("\n") + "\n").as_bytes(),
    );
    let other = vec![claude_line(
        OTHER_SESSION,
        "o1",
        "2026-10-19T09:00:00Z",
        "user",
        json!("unrelated topic"),
    )];
    env.write(
        &format!("claude/proj-beta/{OTHER_SESSION}.jsonl"),
        (other.join("\n") + "\n").as_bytes(),
    );
    let cache = env.path().join("cache");
    let config = env.write_json(
        "skill-recall.json",
        &json!({
            "version": 2,
            "sources": [{
                "source_id": "cc",
                "kind": "claude_code",
                "root": root.to_string_lossy(),
                "enabled": true
            }],
            "source_order": ["cc"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false},
            "cache_root": cache.to_string_lossy()
        }),
    );
    let output = Command::new(bin())
        .arg("index")
        .arg("--config")
        .arg(&config)
        .output()
        .expect("run index");
    assert!(
        output.status.success(),
        "index build failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    Fixture { config }
}

fn run_request(config: &Path, op: &str, request: &str, tag: &str) -> Value {
    let output = Command::new(bin())
        .arg(op)
        .arg("--config")
        .arg(config)
        .arg("--request")
        .arg(format!("@{request}"))
        .output()
        .unwrap_or_else(|error| panic!("run {op} ({tag}): {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{op} example {tag} exited non-zero\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("{op} example {tag} stdout is not JSON ({error}): {stdout}")
    });
    let errors = value["errors"].as_array();
    if let Some(errors) = errors {
        assert!(
            errors.is_empty(),
            "{op} example {tag} returned errors: {errors:?}"
        );
    }
    value
}

fn first_ref(config: &Path) -> Value {
    let request = json!({"query": "needle", "limit": 1});
    let path = std::env::temp_dir().join(format!("am-skill-ref-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_string(&request).unwrap()).unwrap();
    let value = run_request(
        config,
        "memory_search",
        &path.to_string_lossy(),
        "guard-ref",
    );
    let _ = std::fs::remove_file(&path);
    value["results"]
        .as_array()
        .and_then(|results| results.first())
        .map(|hit| hit["ref"].clone())
        .expect("the fixture must produce at least one hit for 'needle'")
}

fn substitute(body: &str, reference: &Value) -> String {
    body.replace("\"$REF\"", &serde_json::to_string(reference).unwrap())
        .replace("$SESSION_ID", SESSION)
}

/// A synthetic knowledge candidate used only by the `$KNOWLEDGE_ID` examples
/// (`learning qualify` / `learning qualified`). The skill never shows
/// `learning ingest`; the guard creates the candidate behind the scenes so the
/// documented command runs against a real stored candidate.
fn knowledge_candidate() -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": "skill guard: explicit roots stay explicit",
        "target_kind": "note",
        "proposed_target": "skills:agent-memory:qualify-guard",
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [{
            "ref": "claude:session-skill-guard:line-1",
            "span_sha256": "0000000000000000000000000000000000000000000000000000000000000003",
            "archive_generation": "g1",
            "observed_at": "2026-11-01T00:00:00Z",
            "actor": "user",
            "session_kind": "interactive",
            "origin_class": "owner",
            "evidence_kind": "user_statement",
            "recall_taint": false
        }],
        "contradicting_evidence": [],
        "dedup": {
            "query": "",
            "matches": [],
            "disposition": "pending",
            "checked_at": null
        },
        "reuse": [],
        "validation": [],
        "promotion": {
            "approval": null,
            "apply": null,
            "verification": null
        },
        "ingestions": [],
        "source_checkpoint": "skill-guard-knowledge",
        "extractor": "skill-guard",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn create_knowledge_candidate(env: &support::TestEnv, root: &Path) -> String {
    let path = env.write(
        "skill-knowledge-candidate.json",
        knowledge_candidate().to_string().as_bytes(),
    );
    let output = Command::new(bin())
        .arg("learning")
        .arg("ingest")
        .arg("--root")
        .arg(root)
        .arg("--candidate")
        .arg(format!("@{}", path.display()))
        .output()
        .expect("create the guard knowledge candidate");
    assert!(
        output.status.success(),
        "guard knowledge ingest failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value =
        serde_json::from_slice(&output.stdout).expect("guard ingest returns JSON");
    value["candidate_id"]
        .as_str()
        .expect("guard candidate id")
        .to_string()
}

#[test]
fn every_marked_example_runs() {
    let env = support::TestEnv::new();
    let fixture = build_fixture(&env);
    let reference = first_ref(&fixture.config);

    let files = skill_files();
    let mut learning_runs: Vec<String> = Vec::new();
    let mut memory_examples = 0;
    let mut learning_root: Option<PathBuf> = None;
    let mut candidate_id: Option<String> = None;
    let mut knowledge_id: Option<String> = None;

    for path in &files {
        let text = read_skill_file(path);
        for example in examples_in(path, &text) {
            match example.op.as_str() {
                "memory_search" | "memory_read" | "memory_recent" => {
                    memory_examples += 1;
                    let request_path = env.write(
                        &format!("example-{memory_examples}.json"),
                        substitute(&example.body, &reference).as_bytes(),
                    );
                    run_request(
                        &fixture.config,
                        &example.op,
                        &request_path.to_string_lossy(),
                        &example.file,
                    );
                }
                "learning" => {
                    let root = learning_root
                        .get_or_insert_with(|| env.path().join("learning"))
                        .clone();
                    if !root.exists() {
                        let output = Command::new(bin())
                            .arg("learning")
                            .arg("init")
                            .arg("--root")
                            .arg(&root)
                            .output()
                            .expect("init learning root");
                        assert!(
                            output.status.success(),
                            "learning init failed: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    let values: Vec<String> =
                        serde_json::from_str(&example.body).unwrap_or_else(|error| {
                            panic!("{} example is not a JSON array: {error}", example.file)
                        });
                    if values.iter().any(|value| value.contains("$KNOWLEDGE_ID"))
                        && knowledge_id.is_none()
                    {
                        knowledge_id = Some(create_knowledge_candidate(&env, &root));
                    }
                    let runnable = if values.first().map(String::as_str) == Some("task") {
                        values.get(1).cloned().unwrap_or_default()
                    } else {
                        values.first().cloned().unwrap_or_default()
                    };
                    learning_runs.push(runnable.clone());
                    let args: Vec<String> = values
                        .iter()
                        .map(|value| {
                            value
                                .replace("$ROOT", &root.to_string_lossy())
                                .replace("$CANDIDATE_ID", candidate_id.as_deref().unwrap_or(""))
                                .replace(
                                    "$KNOWLEDGE_ID",
                                    knowledge_id.as_deref().unwrap_or(""),
                                )
                        })
                        .collect();
                    assert!(
                        !args.iter().any(|arg| arg.is_empty()),
                        "{} example used a placeholder before its value existed",
                        example.file
                    );
                    let output = Command::new(bin())
                        .arg("learning")
                        .args(&args)
                        .output()
                        .expect("run learning example");
                    assert_eq!(
                        output.status.code(),
                        Some(0),
                        "learning example {runnable} ({}) exited non-zero\nstdout: {}\nstderr: {}",
                        example.file,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
                        if let Some(id) = value["candidate_id"].as_str() {
                            candidate_id = Some(id.to_string());
                        }
                    }
                }
                other => panic!("unknown example operation {other} in {}", example.file),
            }
        }
    }

    assert!(
        memory_examples >= 3,
        "expected marked recall examples for all three operations, found {memory_examples}"
    );
    for expected in LEARNING_EXPECTED {
        assert!(
            learning_runs.iter().any(|run| run == expected),
            "missing marked learning example: {expected} (found {learning_runs:?})"
        );
    }
}

// ---------------------------------------------------------------------------
// No write/maintenance command may appear as an executable example
// ---------------------------------------------------------------------------

fn fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            match current.take() {
                Some(block) => blocks.push(block),
                None => current = Some(String::new()),
            }
            continue;
        }
        if let Some(block) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    blocks
}

fn contains_word(haystack: &str, word: &str) -> bool {
    let is_word = |ch: char| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
    let mut offset = 0;
    while let Some(found) = haystack[offset..].find(word) {
        let start = offset + found;
        let end = start + word.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .map(|ch| !is_word(ch))
            .unwrap_or(true);
        let after_ok = haystack[end..]
            .chars()
            .next()
            .map(|ch| !is_word(ch))
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        offset = end;
    }
    false
}

#[test]
fn no_write_or_maintenance_command_is_an_executable_example() {
    for path in skill_files() {
        let text = read_skill_file(&path);
        for block in fenced_blocks(&text) {
            for forbidden in FORBIDDEN_EXAMPLES {
                assert!(
                    !contains_word(&block, forbidden),
                    "{} has an executable example using the forbidden command {forbidden}:\n{block}",
                    path.display()
                );
            }
        }
    }
}
