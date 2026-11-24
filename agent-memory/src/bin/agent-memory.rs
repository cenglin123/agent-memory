//! agent-memory CLI: capture / read / status / coverage / replay / scrub.
//!
//! Machine-readable JSON contract:
//! - stdout carries the command's JSON report (except `read` / `attachment-read`
//!   / `derived-read` without `--out`, which write raw bytes to stdout and the
//!   JSON report to stderr);
//! - failures print {"ok":false,"error":...} to stderr and use a stable exit code.
//!
//! Exit codes: 0 ok · 2 usage · 3 io/state · 4 lock held · 5 access denied ·
//! 6 not found / integrity · 7 capture failed · 8 coverage anomaly (report still
//! on stdout) · 9 replay partial/failed (report still on stdout) · 10 scrub found
//! corruption (report still on stdout) · 11 telemetry disabled or event storage
//! unavailable (`events`).
//!
//! `capture-all` follows the same table: the driver report stays on stdout and
//! the exit code is 7 when any source reported an error; with `--skip-if-locked`
//! a held capture lock is `skipped=locked` and exit 0, without it the lock maps
//! to 4.
//!
//! `compact` reports on stdout and exits 0 on success, 3 when objects failed
//! verification or I/O (the report names them; it never deletes unverified
//! data). `audit-scan` exits 0 when the scan ran, 2 for malformed patterns and
//! 3 when objects could not be read/decoded; its report carries counts only.
//!
//! `compact-records` (the record ledger's canonical rewrite) reports on stdout
//! and exits 0, 3 for I/O and 4 when another compaction holds
//! `locks/records-compact.lock`. `--dry-run` reports what it would do and
//! writes nothing.
//!
//! `--help` / `-h` prints the usage text to stdout and exits 0; `--version` /
//! `-V` prints `agent-memory <crate version>` and exits 0. Both are additive:
//! the exit codes below are unchanged, and a bare invocation still reports usage
//! as an error.
//!
//! Reference return contract:
//! - object readback: `--sha256 <64-hex object address>`;
//! - record readback: `--sha256 <64-hex record sha>` (record and object address
//!   spaces are disjoint in practice; the CLI tries the record index first);
//! - attachment readback: `--capture-id <id> --seq <n>` (stable handle
//!   `<captureId>#<seq>`);
//! - derived readback: `--derived-ref derived:<generation>:<sourceKey>`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agent_memory::archive::{hex_sha256, Archive, ArchiveError};
use agent_memory::attachment::{capture_attachment, AttachmentRequest};
use agent_memory::cost::{self, Cost};
use agent_memory::capture::{capture_source_with_options, CaptureOptions, CaptureReport};
use agent_memory::learning::{
    LearningError, LearningErrorKind, LearningStore, TASK_NOT_ROUTEABLE, TASK_TARGET_KIND,
};
use agent_memory::recall::{self, load_config, ExecutionOptions, Op};
use agent_memory::replay::{self, ReplayIdentity};
use agent_memory::report::ReportConfig;
use agent_memory::scrub;
use serde::Serialize;

const EX_USAGE: u8 = 2;
const EX_IO: u8 = 3;
const EX_LOCK: u8 = 4;
const EX_DENIED: u8 = 5;
const EX_NOTFOUND: u8 = 6;
const EX_CAPTURE: u8 = 7;
const EX_COVERAGE: u8 = 8;
const EX_REPLAY: u8 = 9;
const EX_SCRUB: u8 = 10;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--background` lowers this process's CPU/IO/memory priority for the long
    // scheduled commands only. The guard's `Drop` pairs the mode on every exit
    // path (success, usage error, panic), so a manual run without the flag keeps
    // its normal priority.
    let _background = if wants_background(&args) {
        Some(cost::begin_background())
    } else {
        None
    };
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err((code, msg)) => {
            eprintln!("{}", with_background(serde_json::json!({"ok": false, "error": msg})));
            ExitCode::from(code)
        }
    }
}

/// The commands that accept `--background`, and only with the flag present.
fn wants_background(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("capture-all") | Some("index") | Some("audit-grounding")
    ) && args.iter().any(|arg| arg == "--background")
}

/// Attach the installed `background` block (only present when the command
/// requested the mode) to any JSON this process prints.
fn with_background(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(report) = cost::background_report() {
        if let serde_json::Value::Object(map) = &mut value {
            if let Ok(block) = serde_json::to_value(&report) {
                map.insert("background".to_string(), block);
            }
        }
    }
    value
}

type CliResult<T> = Result<T, (u8, String)>;

fn usage_text() -> String {
    "usage: agent-memory <init|capture|capture-all|read|status|reconcile|coverage|replay|scrub|derived-read|attach|attachments|attachment-read|events|compact-records> \
         --root <runtime-root> [--source <path>] [--provider <claude|codex|opencode>] \
         [--domain <access-domain>] [--sha256 <hex>] [--out <file>] \
         [--config <report-config>] [--parser-version <v>] [--model-version <n>] \
         [--derived-ref <derived:generation:key>] [--skip-if-locked] \
         [--request <json|@file>] [--capture-id <id>] [--seq <n>] \
         [--session-since <rfc3339> (fixed threshold: a rolling value re-baselines \
          every run, and removing the filter re-baselines once for full backfill; \
          pass the same value on --verify runs)] [--rebaseline] [--verify]\n       \
         agent-memory capture-all --config <recall.json> [--root <runtime-root>] \
         [--domain <access-domain>] [--source <id>] [--session-since <rfc3339>] \
         [--plan] [--skip-if-locked] [--background]\n       \
         agent-memory compact --root <runtime-root> [--dry-run]\n       \
         agent-memory compact-records --root <runtime-root> [--dry-run]\n       \
         agent-memory audit-scan --root <runtime-root> --patterns <json|@file>\n       \
         agent-memory pointer --config <file> [--request <json|@file>]
       agent-memory <memory_search|memory_read|memory_recent> --config <file> \
         [--strict-snapshot] [--request <json|@file>] \
         [--hook-session-start [--brief [--log <file>] [--host <claude_code|opencode|codex|dsh|workbuddy|kimi_code|pi|hermes>]] [--now <rfc3339>] [--utc-offset <±HH:MM>]]\n       \
         agent-memory index --config <file> [--source <id>] [--rebuild] [--background]\n       \
         agent-memory index-digest --config <file> [--source <id>] [--json]\n       \
         agent-memory events <record|prune|stats|export> --config <file> \
         [--event <json|@file>] [--hit-id <id>] [--include-query]\n       \
         agent-memory learning <init|ingest|show|list|dedup|reuse|validate> \
         --root <absolute-learning-root> [--candidate <json|@file>] \
         [--candidate-id <id>] [--status candidate] [--include-tasks] \
         [--archive <absolute-archive-root> --domain <access-domain>] \
         [--report <json|@file>] \
         [--disposition <duplicate|related|contradicts|new>] [--reason <text>] \
         [--task <id>] [--outcome <pass|fail>] [--notes <text>] \
         [--actor <host:session>] [--client-seq <n>] [--idempotent] \
         [--reviewer <identity>] [--verdict <pass|fail>] [--report-ref <ref>] \
         [--override-rejected <reason>]\n       \
         agent-memory learning qualify --root <absolute-learning-root> \
         --candidate-id <id> --state <rejected|deferred|cleared> --actor <host:session> \
         [--basis <text>] [--counterexample <text>] [--counterexample-ref <ref>] \
         [--owner <identity>] [--trigger <text>] \
         [--archive <absolute-archive-root> --domain <access-domain>]\n       \
         agent-memory learning qualified --root <absolute-learning-root> \
         [--state <rejected|deferred|cleared>] [--query <text>] [--limit <n>] \
         [--audit] [--json]\n       \
         agent-memory learning workspace --root <absolute-learning-root> \
         [--state <active|done|abandoned|stale|all>] [--stale-days <n>] [--now <rfc3339>] \
         [--text]\n       \
         agent-memory learning inject --root <absolute-learning-root> \
         [--max-tasks <n>] [--max-bytes <n>] [--now <rfc3339>] [--check] [--json]\n       \
         agent-memory learning anomalies --root <absolute-learning-root> \
         [--since-seq <n>] [--now <rfc3339>] [--json]\n       \
         agent-memory learning task <create|claim|abandon|show> --root <absolute-learning-root> \
         (--claim <text> | --candidate-id <id>) --actor <host:session> \
         [--derived-from <external|internal|unknown>] \
         [--note <text>] [--reason <text>] [--stale-days <n>] [--now <rfc3339>] [--text]\n       \
         agent-memory learning migrate --root <absolute-learning-root> --to 3\n       \
         agent-memory learning gate --root <absolute-learning-root> \
         --archive <absolute-archive-root> --domain <access-domain> \
         (--candidate-id <id> | --candidate <json|@file>)\n       \
         agent-memory learning <gate-target|export> --root <absolute-learning-root> \
         --candidate-id <id> [--out <file>] \
         [--archive <absolute-archive-root> --domain <access-domain>]\n       \
         agent-memory mcp --config <file> [--strict-snapshot]\n       \
         agent-memory audit-grounding [--kb-summaries <vault-root>] \
         [--learning-root <absolute-learning-root> [--archive <absolute-archive-root> \
         --domain <access-domain>]] [--conflicts --config <file> [--since <rfc3339>]] \
         [--json] [--limit <n>] [--sample <n> --seed <n>] [--out <file>] [--background]\n       \
         agent-memory eval-recall --config <recall.json> --queries <jsonl> \
         [--repeat <n>] [--against <prior-report.json>] \
         [--groups <sessions,kb,all>] [--json] [--out <file>]\n       \
         agent-memory gate --job <grounding|verify> --state <file> \
         [--idle-minutes <n>] [--max-defer-hours <n>]\n       \
         agent-memory <--help|-h|--version|-V>"
        .to_string()
}

fn usage() -> (u8, String) {
    (EX_USAGE, usage_text())
}

fn opt_value(args: &[String], name: &str) -> CliResult<Option<String>> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return Ok(Some(args.get(i + 1).ok_or_else(|| usage())?.clone()));
        }
        i += 1;
    }
    Ok(None)
}

fn required_opt(args: &[String], name: &str) -> CliResult<String> {
    opt_value(args, name)?.ok_or_else(usage)
}

/// Strip one leading UTF-8 BOM (U+FEFF) from a payload the CLI parses.
///
/// PowerShell 5.1 writes UTF-8 with BOM by default, so every `@file` argument
/// and every stdin body goes through this single function before parsing.
/// Exactly one BOM is removed; the rest of the bytes are untouched.
fn strip_bom(text: String) -> String {
    match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

fn print_json(value: &impl Serialize) {
    let value = with_background(serde_json::to_value(value).unwrap_or(serde_json::Value::Null));
    println!(
        "{}",
        serde_json::to_string_pretty(&value).expect("serialize report")
    );
}

fn err_code(e: &ArchiveError) -> u8 {
    match e {
        ArchiveError::LockHeld(_) => EX_LOCK,
        ArchiveError::AccessDenied(_) => EX_DENIED,
        ArchiveError::NotFound(_) | ArchiveError::Integrity(_) => EX_NOTFOUND,
        _ => EX_IO,
    }
}

fn open_archive(root: &str) -> CliResult<Archive> {
    Archive::open(Path::new(root)).map_err(|e| (err_code(&e), e.to_string()))
}

/// Report config is explicit (`--config`) or the runtime root's
/// `config.local.json`. Never a real home directory: the root is injected.
fn report_config(root: &str, args: &[String]) -> CliResult<ReportConfig> {
    if let Some(path) = opt_value(args, "--config")? {
        let path = PathBuf::from(path);
        return ReportConfig::load(&path)
            .map_err(|error| (EX_USAGE, error))?
            .ok_or_else(|| {
                (
                    EX_USAGE,
                    format!("report config {} does not exist", path.display()),
                )
            });
    }
    let candidate = Path::new(root).join("config.local.json");
    if candidate.exists() {
        return ReportConfig::load(&candidate)
            .map_err(|error| (EX_USAGE, error))?
            .ok_or_else(|| (EX_USAGE, format!("{} is not a file", candidate.display())));
    }
    Ok(ReportConfig::default())
}

fn run_events(args: &[String]) -> CliResult<u8> {
    let Some(subcommand) = args.first() else {
        return Err(usage());
    };
    let config_path = required_opt(args, "--config")?;
    let config =
        load_config(Path::new(&config_path)).map_err(|error| (EX_USAGE, error.to_string()))?;
    let telemetry = config.telemetry.as_ref();
    match subcommand.as_str() {
        "record" => {
            let raw = required_opt(args, "--event")?;
            let text = strip_bom(match raw.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|error| (EX_IO, format!("{path}: {error}")))?,
                None => raw,
            });
            let request: serde_json::Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    let message = format!("event is not valid JSON: {error}");
                    print_json(&serde_json::json!({
                        "ok": false,
                        "events_version": agent_memory::recall::telemetry::EVENTS_SCHEMA_VERSION,
                        "code": agent_memory::recall::telemetry::INVALID_EVENT_CODE,
                        "reason": message,
                        "wrote": false,
                        "error": {
                            "path": "event",
                            "reason": "invalid_json",
                            "message": message,
                        },
                    }));
                    return Ok(agent_memory::recall::telemetry::EVENTS_EXIT_USAGE);
                }
            };
            let (response, code) =
                agent_memory::recall::telemetry::record_event(telemetry, &request);
            print_json(&response);
            Ok(code)
        }
        "prune" => {
            let (response, code) = agent_memory::recall::telemetry::prune_events(telemetry);
            print_json(&response);
            Ok(code)
        }
        "stats" => {
            let hit_id = opt_value(args, "--hit-id")?;
            let (response, code) =
                agent_memory::recall::telemetry::stats_events(telemetry, hit_id.as_deref());
            print_json(&response);
            Ok(code)
        }
        "export" => {
            let include_query = args.iter().any(|arg| arg == "--include-query");
            let (response, lines, code) =
                agent_memory::recall::telemetry::export_events(telemetry, include_query);
            if code == agent_memory::recall::telemetry::EVENTS_EXIT_OK {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                for line in &lines {
                    writeln!(stdout, "{line}").map_err(|e| (EX_IO, e.to_string()))?;
                }
                stdout.flush().map_err(|e| (EX_IO, e.to_string()))?;
            }
            // JSON Lines owns stdout; the envelope goes to stderr.
            eprintln!(
                "{}",
                serde_json::to_string(&response).expect("serialize export summary")
            );
            Ok(code)
        }
        _ => Err(usage()),
    }
}

fn run_learning(args: &[String]) -> CliResult<u8> {
    match run_learning_inner(args) {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    "error": {"code": error.code, "message": error.message},
                })
            );
            Ok(error.exit_code())
        }
    }
}

fn learning_opt(args: &[String], name: &str) -> Result<Option<String>, LearningError> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return match args.get(i + 1) {
                Some(value) => Ok(Some(value.clone())),
                None => Err(LearningError::usage(format!("{name} requires a value"))),
            };
        }
        i += 1;
    }
    Ok(None)
}

fn learning_required(args: &[String], name: &str) -> Result<String, LearningError> {
    learning_opt(args, name)?.ok_or_else(|| LearningError::usage(format!("{name} is required")))
}

fn learning_stale_days(args: &[String]) -> Result<u32, LearningError> {
    match learning_opt(args, "--stale-days")? {
        Some(raw) => raw.trim().parse::<u32>().map_err(|error| {
            LearningError::usage(format!(
                "--stale-days must be a non-negative integer, got {raw:?} ({error})"
            ))
        }),
        None => Ok(agent_memory::learning::DEFAULT_STALE_DAYS),
    }
}

fn learning_now(args: &[String]) -> Result<chrono::DateTime<chrono::FixedOffset>, LearningError> {
    match learning_opt(args, "--now")? {
        Some(raw) => chrono::DateTime::parse_from_rfc3339(raw.trim()).map_err(|error| {
            LearningError::usage(format!(
                "--now must be an RFC 3339 timestamp, got {raw:?} ({error})"
            ))
        }),
        None => Ok(chrono::Utc::now().fixed_offset()),
    }
}

fn learning_opt_usize(args: &[String], name: &str) -> Result<Option<usize>, LearningError> {
    match learning_opt(args, name)? {
        Some(raw) => raw.trim().parse::<usize>().map(Some).map_err(|error| {
            LearningError::usage(format!(
                "{name} must be a non-negative integer, got {raw:?} ({error})"
            ))
        }),
        None => Ok(None),
    }
}

/// `learning inject` (stage 5 §B). Argument and caps errors keep the normal
/// usage exit code; every runtime failure (missing root, locked/v3 store,
/// unreadable row) is swallowed into exit 0 with an empty text stdout — the
/// SessionStart hook contract is "never block, never pollute".
fn run_learning_inject(args: &[String]) -> Result<u8, LearningError> {
    use agent_memory::learning::inject::{error_json, warning_line};

    let root = learning_required(args, "--root")?;
    let caps = agent_memory::learning::inject_caps(
        learning_opt_usize(args, "--max-tasks")?,
        learning_opt_usize(args, "--max-bytes")?,
    )?;
    let now = learning_now(args)?;
    let json = args.iter().any(|arg| arg == "--json");
    let check = args.iter().any(|arg| arg == "--check");

    enum Outcome {
        Report(agent_memory::learning::InjectReport),
        Check(agent_memory::learning::InjectCheckReport),
    }

    let outcome = (|| -> Result<Outcome, LearningError> {
        let store = LearningStore::open_read_only(Path::new(&root))?;
        if check {
            Ok(Outcome::Check(store.inject_check(now)?))
        } else {
            Ok(Outcome::Report(store.inject(caps, now)?))
        }
    })();

    match outcome {
        Ok(Outcome::Report(report)) => {
            if json {
                print_json(&report);
            } else {
                print!("{}", report.text);
            }
            Ok(0)
        }
        Ok(Outcome::Check(report)) => {
            if json {
                print_json(&report);
            } else {
                print!("{}", report.text);
            }
            Ok(0)
        }
        Err(error) => {
            if json {
                print_json(&error_json(&error));
            } else {
                eprintln!("{}", warning_line(&error));
            }
            Ok(0)
        }
    }
}

/// S5: `--archive/--domain` is an optional pair for `gate-target` and
/// `export`. Both or neither; when given the S2 gate is evaluated read-only
/// against the current stored record.
fn learning_optional_gate(
    args: &[String],
    record: &agent_memory::learning::model::CandidateRecord,
) -> Result<Option<agent_memory::learning::GateReport>, LearningError> {
    match (
        learning_opt(args, "--archive")?,
        learning_opt(args, "--domain")?,
    ) {
        (None, None) => Ok(None),
        (Some(archive), Some(domain)) => {
            let domain = domain.trim();
            if domain.is_empty() {
                return Err(LearningError::usage(
                    "--domain must be a non-empty access domain",
                ));
            }
            let archive = agent_memory::learning::gate::open_archive(&archive)?;
            Ok(Some(agent_memory::learning::gate::evaluate(
                record, &archive, domain,
            )?))
        }
        _ => Err(LearningError::usage(
            "--archive and --domain must be given together",
        )),
    }
}

/// `learning` is deliberately separate from the recall surface: it takes an
/// explicit absolute learning root. `init`/`ingest`/`show`/`list`/`dedup` never
/// touch an archive on their own; `ingest --archive/--domain` and `gate`
/// additionally take an explicit committed archive root and only ever read from
/// it.
fn run_learning_inner(args: &[String]) -> Result<u8, LearningError> {
    let Some(subcommand) = args.first() else {
        return Err(LearningError::usage(
            "learning requires a subcommand: init | ingest | show | list | workspace | inject | \
             anomalies | gate | dedup | reuse | validate | qualify | qualified | task | gate-target \
             | export | migrate",
        ));
    };
    match subcommand.as_str() {
        "init" => {
            let root = learning_required(args, "--root")?;
            let store = LearningStore::init(Path::new(&root))?;
            print_json(&serde_json::json!({
                "ok": true,
                "root": store.root().to_string_lossy(),
                "db": store.db_path().to_string_lossy(),
                "schema_version": agent_memory::learning::SCHEMA_VERSION,
                "db_schema_version": agent_memory::learning::DB_SCHEMA_VERSION,
            }));
            Ok(0)
        }
        "ingest" => {
            let root = learning_required(args, "--root")?;
            let raw = learning_required(args, "--candidate")?;
            let text = strip_bom(match raw.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path).map_err(|error| {
                    LearningError::new(
                        "LEARNING_IO",
                        LearningErrorKind::Io,
                        format!("{path}: {error}"),
                    )
                })?,
                None => raw,
            });
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                LearningError::invalid_candidate(format!("candidate is not valid JSON: {error}"))
            })?;

            // Stage 4: a task candidate may only be produced by `task create`,
            // which writes the first claim event atomically. Accepting it here
            // would allow a task with no event at all.
            if value.get("target_kind").and_then(serde_json::Value::as_str)
                == Some(agent_memory::learning::TASK_TARGET_KIND)
            {
                return Err(LearningError::new(
                    agent_memory::learning::TASK_REQUIRES_TASK_CREATE,
                    LearningErrorKind::Usage,
                    "a target_kind \"task\" candidate cannot be ingested directly; use \
                     `agent-memory learning task create --root <R> --claim <T> --actor <A>` so \
                     the task exists together with its first claim event",
                ));
            }

            // S3: the S2 gate is an optional precondition. Both flags or
            // neither; when given, the same candidate is adjudicated against
            // the same committed archive *before* the ingest transaction
            // starts, so a blocked ingest writes nothing.
            match (
                learning_opt(args, "--archive")?,
                learning_opt(args, "--domain")?,
            ) {
                (None, None) => {}
                (Some(archive), Some(domain)) => {
                    let domain = domain.trim();
                    if domain.is_empty() {
                        return Err(LearningError::usage(
                            "--domain must be a non-empty access domain",
                        ));
                    }
                    let validated = agent_memory::learning::model::validate_candidate(&value)?;
                    let archive = agent_memory::learning::gate::open_archive(&archive)?;
                    agent_memory::learning::gate::require_passed(
                        &validated.record,
                        &archive,
                        domain,
                    )?;
                }
                _ => {
                    return Err(LearningError::usage(
                        "learning ingest --archive and --domain must be given together",
                    ))
                }
            }

            // Stage 7 §A: an explicit anti-rebirth override. `--actor` is
            // optional and defaults to the candidate extractor.
            let override_reason = learning_opt(args, "--override-rejected")?;
            if let Some(reason) = &override_reason {
                if reason.trim().is_empty() {
                    return Err(LearningError::usage(
                        "--override-rejected requires a non-empty reason",
                    ));
                }
            }
            let override_actor = learning_opt(args, "--actor")?;

            let mut store = LearningStore::open(Path::new(&root))?;
            let outcome = store.ingest_with_override(
                &value,
                override_reason.as_deref(),
                override_actor.as_deref(),
            )?;
            print_json(&outcome);
            Ok(0)
        }
        "dedup" => {
            // S3: record one dedup disposition. The report is the dedup
            // tool's machine-readable output; only its top level is checked,
            // matches are stored verbatim.
            let root = learning_required(args, "--root")?;
            let candidate_id = learning_required(args, "--candidate-id")?;
            let raw = learning_required(args, "--report")?;
            let disposition = learning_required(args, "--disposition")?;
            let reason = learning_opt(args, "--reason")?;
            let text = strip_bom(match raw.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path).map_err(|error| {
                    LearningError::new(
                        "LEARNING_IO",
                        LearningErrorKind::Io,
                        format!("{path}: {error}"),
                    )
                })?,
                None => raw,
            });
            let report: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                LearningError::new(
                    agent_memory::learning::DEDUP_REPORT_INVALID,
                    LearningErrorKind::Usage,
                    format!("--report is not valid JSON: {error}"),
                )
            })?;
            let request = agent_memory::learning::DedupRequest::parse(
                &candidate_id,
                &report,
                &disposition,
                reason.as_deref(),
            )?;
            let mut store = LearningStore::open(Path::new(&root))?;
            let outcome = store.record_dedup(&request)?;
            print_json(&outcome);
            Ok(0)
        }
        "reuse" => {
            // S5/v4: record that a candidate was applied to one task. A
            // knowledge candidate records one task once (REUSE_TASK_CONFLICT);
            // a task candidate records reuse against itself and may accumulate
            // events. `--actor` is required and `--idempotent` needs
            // `--client-seq`.
            let root = learning_required(args, "--root")?;
            let candidate_id = learning_required(args, "--candidate-id")?;
            let task = learning_required(args, "--task")?;
            let outcome = learning_required(args, "--outcome")?;
            let notes = learning_opt(args, "--notes")?;
            let actor = learning_required(args, "--actor")?;
            let client_seq = match learning_opt(args, "--client-seq")? {
                Some(raw) => Some(raw.trim().parse::<i64>().map_err(|error| {
                    LearningError::usage(format!(
                        "--client-seq must be an integer, got {raw:?} ({error})"
                    ))
                })?),
                None => None,
            };
            let idempotent = args.iter().any(|arg| arg == "--idempotent");
            let request = agent_memory::learning::ReuseRequest::parse_with_idempotency(
                &candidate_id,
                &task,
                &outcome,
                notes.as_deref(),
                &actor,
                client_seq,
                idempotent,
            )?;
            let mut store = LearningStore::open(Path::new(&root))?;
            print_json(&store.record_reuse(&request)?);
            Ok(0)
        }
        "task" => {
            // Stage 4: task create/claim/abandon. Every command is one
            // BEGIN IMMEDIATE transaction inside the store; the CLI only parses
            // and reports.
            let Some(action) = args.get(1).map(String::as_str) else {
                return Err(LearningError::usage(
                    "learning task requires an action: create | claim | abandon",
                ));
            };
            match action {
                "create" => {
                    let root = learning_required(args, "--root")?;
                    let claim = learning_required(args, "--claim")?;
                    let actor = learning_required(args, "--actor")?;
                    let derived_from = learning_opt(args, "--derived-from")?;
                    let mut store = LearningStore::open(Path::new(&root))?;
                    print_json(&store.create_task_with_derived_from(
                        &claim,
                        &actor,
                        derived_from.as_deref(),
                    )?);
                    Ok(0)
                }
                "claim" => {
                    let root = learning_required(args, "--root")?;
                    let candidate_id = learning_required(args, "--candidate-id")?;
                    let actor = learning_required(args, "--actor")?;
                    let note = learning_opt(args, "--note")?;
                    let mut store = LearningStore::open(Path::new(&root))?;
                    print_json(&store.claim_task(&candidate_id, &actor, note.as_deref())?);
                    Ok(0)
                }
                "abandon" => {
                    let root = learning_required(args, "--root")?;
                    let candidate_id = learning_required(args, "--candidate-id")?;
                    let actor = learning_required(args, "--actor")?;
                    let reason = learning_required(args, "--reason")?;
                    let mut store = LearningStore::open(Path::new(&root))?;
                    print_json(&store.abandon_task(&candidate_id, &actor, &reason)?);
                    Ok(0)
                }
                "show" => {
                    // Stage 4: read-only single-task view. The unified event
                    // view (all three event tables) and the projection come
                    // from the same fact source as `learning workspace`.
                    let root = learning_required(args, "--root")?;
                    let candidate_id = learning_required(args, "--candidate-id")?;
                    let stale_days = learning_stale_days(args)?;
                    let now = learning_now(args)?;
                    let text = args.iter().any(|arg| arg == "--text");
                    let store = LearningStore::open(Path::new(&root))?;
                    let report = store.task_show(&candidate_id, now, stale_days)?;
                    if text {
                        print!(
                            "{}",
                            agent_memory::learning::inject::render_task_show_text(&report)
                        );
                    } else {
                        print_json(&report);
                    }
                    Ok(0)
                }
                other => Err(LearningError::usage(format!(
                    "unknown learning task action {other:?}; expected create | claim | abandon | \
                     show"
                ))),
            }
        }
        "workspace" => {
            // Stage 4: read-only projection list over every task candidate.
            // Nothing is written: no event, no clock row, no candidate update.
            let root = learning_required(args, "--root")?;
            let state = learning_opt(args, "--state")?;
            let stale_days = learning_stale_days(args)?;
            let now = learning_now(args)?;
            let text = args.iter().any(|arg| arg == "--text");
            let store = LearningStore::open(Path::new(&root))?;
            let report = store.workspace(state.as_deref(), now, stale_days)?;
            if text {
                print!(
                    "{}",
                    agent_memory::learning::inject::render_workspace_text(&report)
                );
            } else {
                print_json(&report);
            }
            Ok(0)
        }
        "inject" => run_learning_inject(args),
        "anomalies" => {
            // Stage 6 §A: read-only anomaly signals. This is a normal CLI
            // surface (not the hook contract): a missing root / locked store
            // keeps the stable exit codes.
            let root = learning_required(args, "--root")?;
            let since_seq = match learning_opt(args, "--since-seq")? {
                Some(raw) => {
                    let parsed = raw.trim().parse::<i64>().map_err(|error| {
                        LearningError::usage(format!(
                            "--since-seq must be a non-negative integer, got {raw:?} ({error})"
                        ))
                    })?;
                    if parsed < 0 {
                        return Err(LearningError::usage(format!(
                            "--since-seq must be a non-negative integer, got {raw:?}"
                        )));
                    }
                    Some(parsed)
                }
                None => None,
            };
            let now = learning_now(args)?;
            let json = args.iter().any(|arg| arg == "--json");
            let store = LearningStore::open_read_only(Path::new(&root))?;
            let report = store.anomalies(since_seq, now)?;
            if json {
                print_json(&report);
            } else {
                print!("{}", report.text);
            }
            Ok(0)
        }
        "validate" => {
            // S5: record one independent validation round (append-only).
            let root = learning_required(args, "--root")?;
            let candidate_id = learning_required(args, "--candidate-id")?;
            let reviewer = learning_required(args, "--reviewer")?;
            let verdict = learning_required(args, "--verdict")?;
            let report_ref = learning_required(args, "--report-ref")?;
            let notes = learning_opt(args, "--notes")?;
            let request = agent_memory::learning::ValidationRequest::parse(
                &candidate_id,
                &reviewer,
                &verdict,
                &report_ref,
                notes.as_deref(),
            )?;
            let mut store = LearningStore::open(Path::new(&root))?;
            print_json(&store.record_validation(&request)?);
            Ok(0)
        }
        "qualify" => {
            // Stage 7 §A: append one rejection/deferral/clear row.
            let root = learning_required(args, "--root")?;
            let candidate_id = learning_required(args, "--candidate-id")?;
            let state = learning_required(args, "--state")?;
            let actor = learning_required(args, "--actor")?;
            let basis = learning_opt(args, "--basis")?;
            let counterexample = learning_opt(args, "--counterexample")?;
            let counterexample_ref = learning_opt(args, "--counterexample-ref")?;
            let owner = learning_opt(args, "--owner")?;
            let trigger = learning_opt(args, "--trigger")?;
            let request = agent_memory::learning::QualifyRequest::parse(
                &candidate_id,
                &state,
                &actor,
                basis.as_deref(),
                counterexample.as_deref(),
                counterexample_ref.as_deref(),
                owner.as_deref(),
                trigger.as_deref(),
            )?;
            let ref_verified = match (
                learning_opt(args, "--archive")?,
                learning_opt(args, "--domain")?,
            ) {
                (None, None) => false,
                (Some(archive), Some(domain)) => {
                    let domain = domain.trim();
                    if domain.is_empty() {
                        return Err(LearningError::usage(
                            "--domain must be a non-empty access domain",
                        ));
                    }
                    match request.counterexample_ref.as_deref() {
                        Some(reference) => {
                            let archive = agent_memory::learning::gate::open_archive(&archive)?;
                            agent_memory::learning::verify_counterexample_ref(
                                &archive, domain, reference,
                            )?
                        }
                        None => false,
                    }
                }
                _ => {
                    return Err(LearningError::usage(
                        "learning qualify --archive and --domain must be given together",
                    ))
                }
            };
            let mut store = LearningStore::open(Path::new(&root))?;
            print_json(&store.qualify(&request, ref_verified)?);
            Ok(0)
        }
        "qualified" => {
            // Stage 7 §A: read-only list. `--audit` is the 90 AM-QUAL feed and
            // always exits 0 for a well-formed request (the report says PASS
            // or FAIL).
            let root = learning_required(args, "--root")?;
            let state = learning_opt(args, "--state")?;
            let query = learning_opt(args, "--query")?;
            let limit = learning_opt_usize(args, "--limit")?;
            if let Some(limit) = limit {
                if limit == 0 {
                    return Err(LearningError::usage("--limit must be at least 1"));
                }
            }
            let json = args.iter().any(|arg| arg == "--json");
            let audit = args.iter().any(|arg| arg == "--audit");
            let store = LearningStore::open_read_only(Path::new(&root))?;
            if audit {
                let now = learning_now(args)?;
                let report = store.qualification_audit(now)?;
                if json {
                    print_json(&report);
                } else {
                    print!(
                        "{}",
                        agent_memory::learning::render_qualification_audit_text(&report)
                    );
                }
                return Ok(0);
            }
            let report = store.qualified(state.as_deref(), query.as_deref(), limit)?;
            if json {
                print_json(&report);
            } else {
                print!("{}", agent_memory::learning::render_qualified_text(&report));
            }
            Ok(0)
        }
        "gate-target" => {
            // S5: read-only per-target mechanical requirement report.
            let root = learning_required(args, "--root")?;
            let candidate_id = learning_required(args, "--candidate-id")?;
            let store = LearningStore::open(Path::new(&root))?;
            let view = store.show(&candidate_id)?;
            let gate = learning_optional_gate(args, &view.candidate)?;
            let report = agent_memory::learning::routing::evaluate_with_qualification(
                &view.candidate,
                gate.as_ref(),
                Some(&view.qualification),
            );
            print_json(&report);
            Ok(0)
        }
        "export" => {
            // S5: full review package (proposal document, never a promotion).
            let root = learning_required(args, "--root")?;
            let candidate_id = learning_required(args, "--candidate-id")?;
            let out = learning_opt(args, "--out")?;
            let store = LearningStore::open(Path::new(&root))?;
            let view = store.show(&candidate_id)?;
            let gate = learning_optional_gate(args, &view.candidate)?;
            let report = agent_memory::learning::routing::evaluate_with_qualification(
                &view.candidate,
                gate.as_ref(),
                Some(&view.qualification),
            );
            // R11: a task candidate has no review package. The refusal is
            // carried by the target report (`ok:false` + TASK_NOT_ROUTEABLE)
            // and no `--out` file is ever written.
            if view.candidate.target_kind == TASK_TARGET_KIND {
                print_json(&report);
                return Ok(0);
            }
            let now = chrono::Utc::now();
            let review_routing = store.review_routing(
                &view.candidate.candidate_id,
                now.fixed_offset(),
                agent_memory::learning::DEFAULT_STALE_DAYS,
            )?;
            let package = agent_memory::learning::routing::review_package(
                &view,
                report,
                now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                review_routing,
            );
            let Some(raw) = out else {
                print_json(&package);
                return Ok(0);
            };
            let path =
                agent_memory::learning::store::resolve_root(Path::new(&raw)).map_err(|_| {
                    LearningError::usage(format!(
                        "--out must be an explicit absolute path, got {raw:?}"
                    ))
                })?;
            let mut bytes = serde_json::to_vec_pretty(&package).map_err(|error| {
                LearningError::state(format!("review package cannot be serialized: {error}"))
            })?;
            bytes.push(b'\n');
            std::fs::write(&path, bytes).map_err(|error| LearningError::io(&path, error))?;
            Ok(0)
        }
        "show" => {
            let root = learning_required(args, "--root")?;
            let candidate_id = learning_required(args, "--candidate-id")?;
            let store = LearningStore::open(Path::new(&root))?;
            let view = store.show(&candidate_id)?;
            print_json(&view);
            Ok(0)
        }
        "list" => {
            let root = learning_required(args, "--root")?;
            let status = learning_opt(args, "--status")?;
            let include_tasks = args.iter().any(|arg| arg == "--include-tasks");
            let store = LearningStore::open(Path::new(&root))?;
            let report = store.list_with_tasks(status.as_deref(), include_tasks)?;
            print_json(&report);
            Ok(0)
        }
        "migrate" => {
            // Stage 4: the only explicit downgrade. 3 -> 4 stays automatic on
            // open; this command snapshots the store and removes task rows.
            let root = learning_required(args, "--root")?;
            let to = learning_required(args, "--to")?;
            if to.trim() != "3" {
                return Err(LearningError::usage(format!(
                    "learning migrate --to supports only 3 (the v3 -> v4 migration is automatic \
                     on open), got {to:?}"
                )));
            }
            print_json(&LearningStore::downgrade_to_v3(Path::new(&root))?);
            Ok(0)
        }
        "gate" => {
            // S2: read-only source/evidence gate. One explicit learning root
            // (the candidate lives there or arrives as an un-stored draft) and
            // one explicit committed archive root. Nothing is written.
            let root = learning_required(args, "--root")?;
            let archive_root = learning_required(args, "--archive")?;
            let domain = learning_required(args, "--domain")?;
            let candidate_id = learning_opt(args, "--candidate-id")?;
            let candidate_raw = learning_opt(args, "--candidate")?;
            let domain = domain.trim().to_string();
            if domain.is_empty() {
                return Err(LearningError::usage(
                    "--domain must be a non-empty access domain",
                ));
            }
            let record = match (candidate_id, candidate_raw) {
                (Some(id), None) => {
                    let store = LearningStore::open(Path::new(&root))?;
                    store.show(id.trim())?.candidate
                }
                (None, Some(raw)) => {
                    // An un-stored draft is validated exactly like `ingest`
                    // would, but it is never written into the learning root.
                    agent_memory::learning::store::resolve_root(Path::new(&root))?;
                    let text = strip_bom(match raw.strip_prefix('@') {
                        Some(path) => std::fs::read_to_string(path).map_err(|error| {
                            LearningError::new(
                                "LEARNING_IO",
                                LearningErrorKind::Io,
                                format!("{path}: {error}"),
                            )
                        })?,
                        None => raw,
                    });
                    let value: serde_json::Value =
                        serde_json::from_str(&text).map_err(|error| {
                            LearningError::invalid_candidate(format!(
                                "candidate is not valid JSON: {error}"
                            ))
                        })?;
                    agent_memory::learning::model::validate_candidate(&value)?.record
                }
                _ => {
                    return Err(LearningError::usage(
                        "learning gate takes exactly one of --candidate-id or --candidate",
                    ))
                }
            };
            // R11: a task candidate is not a gate subject. The guard runs
            // before any archive is opened (and before any write), so a task
            // with a bogus/missing --archive still reports the isolation code.
            if record.target_kind == TASK_TARGET_KIND {
                return Err(LearningError::new(
                    TASK_NOT_ROUTEABLE,
                    LearningErrorKind::Usage,
                    format!(
                        "candidate {} is a workspace task (target_kind \"task\"); `learning \
                         gate` applies only to knowledge candidates",
                        record.candidate_id
                    ),
                ));
            }
            let archive = agent_memory::learning::gate::open_archive(&archive_root)?;
            let report = agent_memory::learning::gate::evaluate(&record, &archive, &domain)?;
            print_json(&report);
            Ok(0)
        }
        other => Err(LearningError::usage(format!(
            "unknown learning subcommand {other:?}; expected init | ingest | show | list | \
             workspace | anomalies | gate | dedup | reuse | validate | qualify | qualified | task | \
             gate-target | export | migrate"
        ))),
    }
}

fn hook_opt(args: &[String], name: &str) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        if args[index] == name {
            return args.get(index + 1).cloned();
        }
        index += 1;
    }
    None
}

/// `--utc-offset <±HH:MM>`: a test-only seam that pins the local zone the
/// opening block renders. Production never passes it (`chrono::Local` applies).
fn parse_utc_offset(raw: &str) -> Result<i32, String> {
    let text = raw.trim();
    let bytes = text.as_bytes();
    let sign = match bytes.first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => {
            return Err(format!(
                "--utc-offset must look like +HH:MM or -HH:MM: {raw}"
            ))
        }
    };
    if bytes.len() != 6 || bytes[3] != b':' {
        return Err(format!(
            "--utc-offset must look like +HH:MM or -HH:MM: {raw}"
        ));
    }
    let digits = |slice: &[u8]| -> Option<i32> {
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse::<i32>().ok()
    };
    let (Some(hours), Some(minutes)) = (digits(&bytes[1..3]), digits(&bytes[4..6])) else {
        return Err(format!(
            "--utc-offset must look like +HH:MM or -HH:MM: {raw}"
        ));
    };
    if hours > 23 || minutes > 59 {
        return Err(format!("--utc-offset is out of range: {raw}"));
    }
    Ok(sign * (hours * 60 + minutes))
}

/// `memory_recent --hook-session-start` (plan §B). Runtime failures are
/// "never block, never pollute": they exit 0, stdout is either the exact block
/// or empty, and stderr carries at most one line. Malformed usage follows the
/// CLI's exit-2 path. With `--brief` the block is replaced by the opening
/// brief (implementation-001); `--brief`, `--log` and `--host` are only legal
/// here, and `--host` additionally requires `--brief` (implementation-002A).
fn run_recent_hook(args: &[String]) -> CliResult<u8> {
    let brief = args.iter().any(|arg| arg == "--brief");
    // `--host` requires `--brief`; reject this usage error before any
    // runtime fail-safe parsing so it cannot be hidden by another bad flag.
    if args
        .iter()
        .any(|arg| arg == "--host" || arg.starts_with("--host="))
        && !brief
    {
        return Err(usage());
    }
    let config = hook_opt(args, "--config");
    let now_ms = match hook_opt(args, "--now") {
        Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw.trim()) {
            Ok(stamp) => Some(stamp.timestamp_millis()),
            Err(error) => {
                let outcome = agent_memory::recall::recent_hook::HookOutcome::failure(&format!(
                    "--now must be an RFC 3339 timestamp: {error}"
                ));
                emit_hook_outcome(&outcome);
                return Ok(0);
            }
        },
        None => None,
    };
    let utc_offset_minutes = match hook_opt(args, "--utc-offset") {
        Some(raw) => match parse_utc_offset(&raw) {
            Ok(minutes) => Some(minutes),
            Err(message) => {
                emit_hook_outcome(&agent_memory::recall::recent_hook::HookOutcome::failure(
                    &message,
                ));
                return Ok(0);
            }
        },
        None => None,
    };
    // `--host` names the caller; inside the hook it is only legal with
    // `--brief` (that is the only run shape that prints a host name), and a
    // value outside `HOOK_HOSTS` fails safe like every other malformed hook
    // invocation.
    let host = match parse_hook_host(args) {
        HookHost::None => agent_memory::recall::recent_hook::HOOK_HOST.to_string(),
        HookHost::Some(id) => id,
        HookHost::Invalid => {
            emit_hook_outcome(&agent_memory::recall::recent_hook::HookOutcome::failure(
                &format!(
                    "--host must be one of: {}",
                    agent_memory::recall::recent_hook::HOOK_HOSTS.join(", ")
                ),
            ));
            return Ok(0);
        }
    };
    let log_path = match parse_hook_log_path(args) {
        HookLogPath::None => None,
        HookLogPath::Some(path) => Some(path),
        HookLogPath::Invalid => {
            emit_hook_outcome(&agent_memory::recall::recent_hook::HookOutcome::failure(
                "--log requires a non-empty path",
            ));
            return Ok(0);
        }
    };
    let outcome = if brief {
        let run = agent_memory::recall::brief::run_session_start_brief(
            config.as_deref().map(Path::new),
            now_ms,
            utc_offset_minutes,
            hook_timeout(),
            hook_test_delay(),
            &host,
        );
        // The host gets its block first; the usage line can never delay or
        // change it.
        emit_hook_outcome(&run.outcome);
        if let Some(path) = log_path.as_deref() {
            append_hook_log_line(path, &run.log_line);
        }
        return Ok(0);
    } else {
        if log_path.is_some() {
            // The usage log only describes a brief run; without `--brief`
            // there is nothing it could describe, so fail safe like any other
            // malformed hook invocation.
            emit_hook_outcome(&agent_memory::recall::recent_hook::HookOutcome::failure(
                "--log is only valid together with --brief",
            ));
            return Ok(0);
        }
        agent_memory::recall::recent_hook::run_session_start(
            config.as_deref().map(Path::new),
            now_ms,
            utc_offset_minutes,
            hook_timeout(),
            hook_test_delay(),
        )
    };
    emit_hook_outcome(&outcome);
    Ok(0)
}

/// Appends one usage line. Every failure is swallowed: the log can never
/// affect the hook's output, exit code or stderr.
fn append_hook_log_line(path: &Path, line: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = file.write_all(format!("{line}\n").as_bytes());
}

/// `--log <path>`: present means "a value is required"; an empty or missing
/// value is a fail-safe usage error inside the hook (the caller emits the
/// single stderr line and exits 0).
enum HookLogPath {
    None,
    Some(PathBuf),
    Invalid,
}

fn parse_hook_log_path(args: &[String]) -> HookLogPath {
    let flag_present = args.iter().any(|arg| arg == "--log");
    match (flag_present, hook_opt(args, "--log")) {
        (false, _) => HookLogPath::None,
        (true, Some(raw)) if !raw.trim().is_empty() => HookLogPath::Some(PathBuf::from(raw)),
        (true, _) => HookLogPath::Invalid,
    }
}

/// `--host <id>`: present means "a value is required"; a missing, empty or
/// non-whitelisted value is a fail-safe usage error inside the hook (the
/// caller emits the single stderr line and exits 0). Only an exact
/// `HOOK_HOSTS` member (after trimming) reaches the output.
enum HookHost {
    None,
    Some(String),
    Invalid,
}

fn parse_hook_host(args: &[String]) -> HookHost {
    let flag_present = args
        .iter()
        .any(|arg| arg == "--host" || arg.starts_with("--host="));
    match (flag_present, hook_opt(args, "--host")) {
        (false, _) => HookHost::None,
        (true, Some(raw)) => {
            let id = raw.trim();
            if agent_memory::recall::recent_hook::HOOK_HOSTS.contains(&id) {
                HookHost::Some(id.to_string())
            } else {
                HookHost::Invalid
            }
        }
        (true, None) => HookHost::Invalid,
    }
}

/// The 3 s hard deadline can only be lowered, never raised. The override is a
/// test seam for the timeout path (R6); production leaves it unset.
fn hook_timeout() -> std::time::Duration {
    let cap = agent_memory::recall::recent_hook::HOOK_TIMEOUT_MS;
    let millis = std::env::var("AGENT_MEMORY_HOOK_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(cap)
        .clamp(1, cap);
    std::time::Duration::from_millis(millis)
}

/// Test seam for the timeout path: a worker delay before the query runs.
fn hook_test_delay() -> Option<std::time::Duration> {
    let millis = std::env::var("AGENT_MEMORY_HOOK_TEST_DELAY_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())?;
    Some(std::time::Duration::from_millis(millis.min(10_000)))
}

fn emit_hook_outcome(outcome: &agent_memory::recall::recent_hook::HookOutcome) {
    if !outcome.stdout.is_empty() {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(outcome.stdout.as_bytes());
        let _ = stdout.flush();
    }
    if let Some(line) = &outcome.stderr {
        eprintln!("{line}");
    }
}

/// Parse a positive (`>= 1`) usize option. `None` when the flag is absent.
fn positive_usize_opt(args: &[String], name: &str) -> CliResult<Option<usize>> {
    match opt_value(args, name)? {
        Some(raw) => {
            let value = raw.trim().parse::<usize>().map_err(|error| {
                (EX_USAGE, format!("{name} must be a positive integer ({error})"))
            })?;
            if value == 0 {
                return Err((EX_USAGE, format!("{name} must be at least 1")));
            }
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

/// Stage 7 §B: `audit-grounding`. Read-only over explicit inputs; the only
/// possible write is the atomic `--out` report. Exit 0 all grounded, 1 any
/// ungrounded item, 2 any usage/runtime error.
fn run_audit_grounding(args: &[String]) -> CliResult<u8> {
    use agent_memory::grounding::{audit_grounding, audit_grounding_to_file, GroundingOptions};

    let since = match opt_value(args, "--since")? {
        Some(raw) => Some(
            chrono::DateTime::parse_from_rfc3339(raw.trim()).map_err(|error| {
                (EX_USAGE, format!("--since must be an RFC 3339 timestamp ({error})"))
            })?,
        ),
        None => None,
    };
    let seed = match opt_value(args, "--seed")? {
        Some(raw) => Some(raw.trim().parse::<u64>().map_err(|error| {
            (EX_USAGE, format!("--seed must be a non-negative integer ({error})"))
        })?),
        None => None,
    };
    let options = GroundingOptions {
        kb_summaries: opt_value(args, "--kb-summaries")?.map(PathBuf::from),
        learning_root: opt_value(args, "--learning-root")?.map(PathBuf::from),
        archive: opt_value(args, "--archive")?,
        domain: opt_value(args, "--domain")?,
        conflicts: args.iter().any(|arg| arg == "--conflicts"),
        config: opt_value(args, "--config")?.map(PathBuf::from),
        since,
        limit: positive_usize_opt(args, "--limit")?,
        sample: positive_usize_opt(args, "--sample")?,
        seed,
    };
    // `--json` is accepted for CLI parity: this report has exactly one format.
    let report = match opt_value(args, "--out")? {
        Some(raw) if raw.trim().is_empty() => {
            return Err((EX_USAGE, "--out must not be empty".to_string()))
        }
        Some(raw) => audit_grounding_to_file(&options, Path::new(&raw))
            .map_err(|error| (EX_USAGE, error.to_string()))?,
        None => audit_grounding(&options).map_err(|error| (EX_USAGE, error.to_string()))?,
    };
    print_json(&report);
    Ok(report.exit_code())
}

/// Stage 7 §C: `eval-recall`. Read-only; the only write is the explicit
/// atomic `--out` report. Exit 0 completed, 2 usage / invalid question set /
/// corpus changed / runtime failure.
fn run_eval_recall(args: &[String]) -> CliResult<u8> {
    use agent_memory::recall::eval::{
        eval_recall, EvalError, EvalOptions, Group, EVAL_GROUPS, EVAL_REPEAT_MAX,
    };

    // This new surface is strict: an unknown flag is a usage error instead of
    // being silently ignored (a typo must never look like a measured run).
    let known = [
        "--config",
        "--queries",
        "--repeat",
        "--against",
        "--groups",
        "--json",
        "--out",
    ];
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        if token == "--json" {
            index += 1;
            continue;
        }
        if token.starts_with("--") {
            if !known.contains(&token.as_str()) {
                return Err((EX_USAGE, format!("unknown option {token:?}")));
            }
            index += 2;
        } else {
            return Err((EX_USAGE, format!("unexpected argument {token:?}")));
        }
    }

    let config_path = required_opt(args, "--config")?;
    let queries = required_opt(args, "--queries")?;
    let repeat = positive_usize_opt(args, "--repeat")?.unwrap_or(1);
    if repeat > EVAL_REPEAT_MAX {
        return Err((
            EX_USAGE,
            format!("--repeat must be at most {EVAL_REPEAT_MAX}"),
        ));
    }
    let groups = match opt_value(args, "--groups")? {
        None => EVAL_GROUPS
            .iter()
            .map(|name| Group::parse(name).expect("group name"))
            .collect::<Vec<_>>(),
        Some(raw) => {
            let mut parsed = Vec::new();
            for name in raw.split(',') {
                let name = name.trim();
                let group = Group::parse(name).ok_or_else(|| {
                    (
                        EX_USAGE,
                        format!(
                            "--groups must list {}; got {name:?}",
                            EVAL_GROUPS.join(",")
                        ),
                    )
                })?;
                if parsed.contains(&group) {
                    return Err((EX_USAGE, format!("--groups repeats {name:?}")));
                }
                parsed.push(group);
            }
            if parsed.is_empty() {
                return Err((EX_USAGE, "--groups must not be empty".to_string()));
            }
            parsed
        }
    };
    let against = opt_value(args, "--against")?.map(PathBuf::from);
    let out = opt_value(args, "--out")?;
    let config = load_config(Path::new(&config_path))
        .map_err(|error| (EX_USAGE, error.to_string()))?;
    let options = EvalOptions {
        queries_path: PathBuf::from(&queries),
        repeat,
        against,
        groups,
    };
    let report = match eval_recall(&config, &options) {
        Ok(report) => report,
        Err(EvalError::Queries(issues)) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    "command": "eval-recall",
                    "error": {
                        "code": "INVALID_QUERIES",
                        "message": "the question set is invalid; nothing ran",
                        "errors": issues,
                    },
                })
            );
            return Ok(EX_USAGE);
        }
        Err(EvalError::Runtime { code, message }) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    "command": "eval-recall",
                    "error": { "code": code, "message": message },
                })
            );
            return Ok(EX_USAGE);
        }
    };
    if let Some(raw) = out {
        if raw.trim().is_empty() {
            return Err((EX_USAGE, "--out must not be empty".to_string()));
        }
        // A bare file name is relative to the current directory; the atomic
        // writer needs an explicit existing parent.
        let path = PathBuf::from(&raw);
        let path = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => path,
            _ => std::env::current_dir()
                .map_err(|error| (EX_IO, format!("current directory: {error}")))?
                .join(&path),
        };
        let mut bytes = serde_json::to_vec_pretty(&report)
            .map_err(|error| (EX_USAGE, format!("report cannot be serialized: {error}")))?;
        bytes.push(b'\n');
        agent_memory::grounding::write_atomic(&path, &bytes)
            .map_err(|error| (EX_USAGE, error.to_string()))?;
    }
    print_json(&report);
    Ok(report.exit_code())
}

fn run(args: &[String]) -> CliResult<u8> {
    let Some(cmd) = args.first() else {
        return Err(usage());
    };
    // Additive surface: help/version are informational and exit 0, so an offline
    // installation check does not have to treat a usage error as success.
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            println!("{}", usage_text());
            return Ok(0);
        }
        "-V" | "--version" | "version" => {
            println!("agent-memory {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        _ => {}
    }
    match cmd.as_str() {
        "init" => {
            let root = required_opt(args, "--root")?;
            let archive = Archive::init(Path::new(&root)).map_err(|e| (EX_IO, e.to_string()))?;
            let schema = archive.schema().map_err(|e| (EX_IO, e.to_string()))?;
            print_json(&serde_json::json!({
                "ok": true,
                "root": archive.root(),
                "deviceId": archive.device_id().map_err(|e| (EX_IO, e.to_string()))?,
                "schema": schema,
            }));
            Ok(0)
        }
        "capture" => {
            let root = required_opt(args, "--root")?;
            let source = required_opt(args, "--source")?;
            let provider = required_opt(args, "--provider")?;
            let domain = opt_value(args, "--domain")?.unwrap_or_else(|| "default".to_string());
            // SQLite options (ignored by non-SQLite sources).
            let session_since_ms = match opt_value(args, "--session-since")? {
                Some(raw) => Some(
                    chrono::DateTime::parse_from_rfc3339(&raw)
                        .map_err(|err| {
                            (
                                EX_USAGE,
                                format!("--session-since must be RFC 3339 ({err})"),
                            )
                        })?
                        .timestamp_millis(),
                ),
                None => None,
            };
            let options = CaptureOptions {
                session_since_ms,
                rebaseline: args.iter().any(|arg| arg == "--rebaseline"),
                verify: args.iter().any(|arg| arg == "--verify"),
                sqlite_profile: None,
            };
            let archive = open_archive(&root)?;
            let report = capture_source_with_options(
                &archive,
                Path::new(&source),
                &provider,
                &domain,
                None,
                options,
            )
            .map_err(|e| (err_code(&e), e.to_string()))?;
            print_json(&report);
            if report.ok {
                Ok(0)
            } else {
                Err((
                    EX_CAPTURE,
                    report
                        .error
                        .clone()
                        .unwrap_or_else(|| "capture failed".to_string()),
                ))
            }
        }
        "capture-all" => {
            // Multi-source driver: enumerates the enabled live sources from the
            // explicit recall.json, captures each file through the same engine
            // as `capture` (per-file single-writer lock), and keeps its own
            // skip ledger at <root>/state/capture-all.json. The report on
            // stdout carries metadata only (paths, counts, error codes).
            let config_path = required_opt(args, "--config")?;
            let config = load_config(Path::new(&config_path))
                .map_err(|error| (EX_USAGE, error.to_string()))?;
            let options = agent_memory::capture_all::CaptureAllOptions {
                root: opt_value(args, "--root")?,
                domain: opt_value(args, "--domain")?.unwrap_or_else(|| "default".to_string()),
                only_source: opt_value(args, "--source")?,
                session_since: opt_value(args, "--session-since")?,
                plan: args.iter().any(|arg| arg == "--plan"),
                skip_if_locked: args.iter().any(|arg| arg == "--skip-if-locked"),
            };
            let (report, code) = agent_memory::capture_all::capture_all(&config, &options)?;
            print_json(&report);
            Ok(code)
        }
        "read" => {
            let root = required_opt(args, "--root")?;
            let sha = required_opt(args, "--sha256")?;
            let domain = required_opt(args, "--domain")?;
            let out = opt_value(args, "--out")?;
            let archive = open_archive(&root)?;
            let handle = archive.reader(&domain);

            // Records first (a record sha is not an object sha; disjoint spaces
            // in practice), then whole objects.
            let records = archive
                .load_records()
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let is_record = records.records.contains_key(&sha);
            let (kind, bytes) = if is_record {
                ("record", handle.read_record(&sha))
            } else {
                ("object", handle.read_object(&sha))
            };
            let bytes = bytes.map_err(|e| (err_code(&e), e.to_string()))?;
            let verified = agent_memory::archive::hex_sha256(&bytes) == sha;
            if !verified {
                return Err((EX_NOTFOUND, format!("readback hash mismatch for {sha}")));
            }
            if let Some(out) = out {
                std::fs::write(&out, &bytes).map_err(|e| (EX_IO, format!("{out}: {e}")))?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "kind": kind,
                    "sha256": sha,
                    "bytes": bytes.len(),
                    "verified": true,
                    "out": out,
                }));
            } else {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(&bytes)
                    .and_then(|_| stdout.flush())
                    .map_err(|e| (EX_IO, e.to_string()))?;
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "ok": true, "kind": kind, "sha256": sha,
                        "bytes": bytes.len(), "verified": true,
                    })
                );
            }
            Ok(0)
        }
        "memory_search" | "memory_read" | "memory_recent" => {
            // SessionStart hook mode (plan §B): a dedicated, fail-safe path.
            // It never reads `--request`, never prints JSON and always exits 0.
            if cmd == "memory_recent" && args.iter().any(|arg| arg == "--hook-session-start") {
                return run_recent_hook(args);
            }
            // implementation-001/002A: `--brief` / `--log` / `--host` belong
            // to the SessionStart hook only; anywhere else they are a usage
            // error.
            if cmd == "memory_recent"
                && args
                    .iter()
                    .any(|arg| {
                        arg == "--brief"
                            || arg == "--log"
                            || arg == "--host"
                            || arg.starts_with("--host=")
                    })
            {
                return Err(usage());
            }
            let op = match cmd.as_str() {
                "memory_search" => Op::Search,
                "memory_read" => Op::Read,
                _ => Op::Recent,
            };
            let config_path = required_opt(args, "--config")?;
            let strict = args.iter().any(|arg| arg == "--strict-snapshot");
            let raw = match opt_value(args, "--request")? {
                Some(raw) => raw,
                None => {
                    use std::io::Read;
                    let mut buffer = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buffer)
                        .map_err(|e| (EX_IO, format!("stdin: {e}")))?;
                    buffer
                }
            };
            let text = strip_bom(match raw.strip_prefix('@') {
                Some(path) => {
                    std::fs::read_to_string(path).map_err(|e| (EX_IO, format!("{path}: {e}")))?
                }
                None => raw,
            });
            // `memory_recent` has an all-optional request: an omitted
            // `--request` (and an empty stdin) means the documented defaults.
            let text = if op == Op::Recent && text.trim().is_empty() {
                "{}".to_string()
            } else {
                text
            };
            let request: serde_json::Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(err) => {
                    let executed = recall::ops::invalid_request(
                        op,
                        "request",
                        "invalid_json",
                        &format!("request is not valid JSON: {err}"),
                    );
                    print_json(&executed.value);
                    return Ok(executed.exit_code as u8);
                }
            };
            let executed = match load_config(Path::new(&config_path)) {
                Ok(config) => recall::execute(
                    &config,
                    op,
                    &request,
                    &ExecutionOptions {
                        strict_snapshot: strict,
                    },
                ),
                Err(error) => recall::ops::config_failure(op, &error),
            };
            print_json(&executed.value);
            Ok(executed.exit_code as u8)
        }
        "pointer" => {
            // implementation-005B: one bounded, model-free pointer decision.
            let config_path = required_opt(args, "--config")?;
            let raw = match opt_value(args, "--request")? {
                Some(raw) => raw,
                None => {
                    use std::io::Read;
                    let mut buffer = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buffer)
                        .map_err(|e| (EX_IO, format!("stdin: {e}")))?;
                    buffer
                }
            };
            let text = strip_bom(match raw.strip_prefix('@') {
                Some(path) => {
                    std::fs::read_to_string(path).map_err(|e| (EX_IO, format!("{path}: {e}")))?
                }
                None => raw,
            });
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|error| (EX_USAGE, format!("request is not valid JSON: {error}")))?;
            let request = agent_memory::recall::pointer::PointerRequest::parse(&value)
                .map_err(|error| (EX_USAGE, error.to_string()))?;
            let config = load_config(Path::new(&config_path))
                .map_err(|error| (EX_USAGE, error.to_string()))?;
            let response = agent_memory::recall::pointer::run(&config, &request);
            print_json(&response.to_value());
            Ok(0)
        }
        "mcp" => {
            let config_path = required_opt(args, "--config")?;
            let strict = args.iter().any(|arg| arg == "--strict-snapshot");
            let code = agent_memory::mcp::run_stdio(Path::new(&config_path), strict);
            Ok(code as u8)
        }
        "index" => {
            // `index` is the only command that writes the live full-text cache.
            // It never opens a session database for writing and never guesses a
            // home directory: `cache_root` must be in the configuration.
            let config_path = required_opt(args, "--config")?;
            let source = opt_value(args, "--source")?;
            let rebuild = args.iter().any(|arg| arg == "--rebuild");
            let config = load_config(Path::new(&config_path))
                .map_err(|error| (EX_USAGE, error.to_string()))?;
            let started = std::time::Instant::now();
            let start_io = cost::sample_io().ok();
            // Stage 7 §C rework: disclose the eval pause marker (active: every
            // indexable source is skipped below; stale: ignored, never deleted).
            let pause_root = agent_memory::eval_pause::configured_cache_root(&config);
            let pause_state = pause_root
                .as_deref()
                .map(|root| {
                    agent_memory::eval_pause::inspect(
                        root,
                        agent_memory::eval_pause::now_unix_ms(),
                    )
                })
                .unwrap_or(agent_memory::eval_pause::PauseState::Absent);
            let mut warnings: Vec<&str> = Vec::new();
            if matches!(
                pause_state,
                agent_memory::eval_pause::PauseState::Stale { .. }
            ) {
                warnings.push("stale_eval_pause");
            }
            let pause_report = agent_memory::eval_pause::report(
                pause_root.as_deref(),
                &pause_state,
            );
            match agent_memory::recall::fulltext::build::index_all(
                &config,
                source.as_deref(),
                rebuild,
            ) {
                Ok(reports) => {
                    let total_rows: u64 = reports.iter().map(|report| report.rows).sum();
                    let ok = reports.iter().all(|report| {
                        matches!(
                            report.status.as_str(),
                            "built"
                                | "updated"
                                | "unchanged"
                                | "skipped_disabled"
                                | "skipped_not_indexable"
                                | "skipped_eval_in_progress"
                        )
                    });
                    print_json(&serde_json::json!({
                        "ok": ok,
                        "cache_root": config.cache_root,
                        "sources": reports,
                        "total_rows": total_rows,
                        "duration_ms": started.elapsed().as_millis() as u64,
                        "cost": Cost::sample(
                            started.elapsed().as_millis() as u64,
                            start_io,
                            cost::sample_io().ok(),
                        ),
                        "eval_pause": pause_report,
                        "warnings": warnings,
                    }));
                    if ok {
                        Ok(0)
                    } else {
                        Ok(EX_IO)
                    }
                }
                Err(error) => {
                    print_json(&serde_json::json!({
                        "ok": false,
                        "code": error.code,
                        "message": error.message,
                        "detail": error.detail,
                    }));
                    Ok(EX_USAGE)
                }
            }
        }
        "index-digest" => {
            // §G.1 read-only oracle: normalised digests of the live index, for
            // comparing an incrementally maintained cache against a rebuild.
            // Never writes, never takes the index write lock.
            let config_path = required_opt(args, "--config")?;
            let source = opt_value(args, "--source")?;
            let as_json = args.iter().any(|arg| arg == "--json");
            let config = load_config(Path::new(&config_path))
                .map_err(|error| (EX_USAGE, error.to_string()))?;
            if let Some(id) = source.as_deref() {
                if !config.sources.iter().any(|entry| entry.source_id == id) {
                    return Err((EX_USAGE, format!("unknown source {id:?}")));
                }
            }
            let digests = agent_memory::recall::fulltext::digest::digest_sources(
                &config,
                source.as_deref(),
            );
            // Sources without a live index (kb, archive, disabled) report `not_indexable`;
            // they are not a failure. Missing, stale or corrupt indexes still are.
            let ok = digests
                .iter()
                .all(|digest| digest.status == "ready" || digest.status == "not_indexable");
            if as_json {
                print_json(&serde_json::json!({
                    "ok": ok,
                    "cache_root": config.cache_root,
                    "exemptions": agent_memory::recall::fulltext::digest::DIGEST_EXEMPT_FIELDS,
                    "sources": digests,
                }));
            } else {
                for digest in &digests {
                    match (&digest.units, &digest.doc_keys) {
                        (Some(units), Some(doc_keys)) => println!(
                            "{}\tstatus={}\tunits={}\tunits_digest={}\tdoc_keys={}\tdoc_keys_digest={}",
                            digest.source_id,
                            digest.status,
                            units.count,
                            units.digest,
                            doc_keys.count,
                            doc_keys.digest
                        ),
                        _ => println!(
                            "{}\tstatus={}\treason={}",
                            digest.source_id,
                            digest.status,
                            digest.reason.as_deref().unwrap_or("")
                        ),
                    }
                }
            }
            if ok {
                Ok(0)
            } else {
                Ok(EX_IO)
            }
        }
        "events" => run_events(&args[1..]),
        "learning" => run_learning(&args[1..]),
        "audit-grounding" => run_audit_grounding(&args[1..]),
        "eval-recall" => run_eval_recall(&args[1..]),
        "coverage" => {
            let root = required_opt(args, "--root")?;
            let archive = open_archive(&root)?;
            let config = report_config(&root, args)?;
            let report = archive
                .coverage_with(&config)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            print_json(&report);
            if report.ok {
                Ok(0)
            } else {
                // The report itself is the payload; the exit code only says
                // "current-view anomaly present".
                Ok(EX_COVERAGE)
            }
        }
        "replay" => {
            let root = required_opt(args, "--root")?;
            let domain = required_opt(args, "--domain")?;
            let archive = open_archive(&root)?;
            let mut identity = ReplayIdentity::current();
            if let Some(version) = opt_value(args, "--parser-version")? {
                if version.trim().is_empty() {
                    return Err((EX_USAGE, "--parser-version must not be empty".to_string()));
                }
                identity.parser_version = version;
            }
            if let Some(model) = opt_value(args, "--model-version")? {
                identity.model_version = model
                    .parse()
                    .map_err(|_| (EX_USAGE, "--model-version must be an integer".to_string()))?;
            }
            // An explicit configuration may carry `interpreter.embedding`. Its
            // absence keeps the P1 lexical replay. A model failure falls back to
            // the lexical index and reports the degradation instead of failing
            // the whole replay.
            let embedding = match opt_value(args, "--config")? {
                Some(path) => {
                    let config = load_config(Path::new(&path))
                        .map_err(|error| (EX_USAGE, error.to_string()))?;
                    agent_memory::recall::semantic::EmbeddingPlan::from_config(&config)
                }
                None => None,
            };
            let mut report = match embedding.as_ref() {
                Some(plan) => match replay::replay_with_plan(&archive, &identity, &domain, plan) {
                    Ok(report) => report,
                    Err(error @ ArchiveError::Embedding(_)) => {
                        let mut report = replay::replay(&archive, &identity, &domain)
                            .map_err(|e| (err_code(&e), e.to_string()))?;
                        let message = error.to_string();
                        report.anomalies.push(format!(
                            "semantic generation degraded: {message}; lexical index rebuilt"
                        ));
                        report.semantic = Some(replay::ReplaySemanticStatus {
                            status: "degraded".to_string(),
                            reason: Some(message.clone()),
                            backend: plan.backend().to_string(),
                            generation_key: plan.generation_key(),
                            dimension: plan.spec.dimension,
                            units: 0,
                            detail: Some(message),
                        });
                        report
                    }
                    Err(error) => return Err((err_code(&error), error.to_string())),
                },
                None => replay::replay(&archive, &identity, &domain)
                    .map_err(|e| (err_code(&e), e.to_string()))?,
            };
            if report.semantic.is_none() {
                if let Some(spec) = embedding.as_ref() {
                    report.semantic = Some(replay::ReplaySemanticStatus {
                        status: "disabled".to_string(),
                        reason: Some("embedding_not_applied".to_string()),
                        backend: spec.backend().to_string(),
                        generation_key: spec.generation_key(),
                        dimension: spec.spec.dimension,
                        units: 0,
                        detail: None,
                    });
                }
            }
            print_json(&report);
            if report.ok {
                Ok(0)
            } else {
                Ok(EX_REPLAY)
            }
        }
        "scrub" => {
            let root = required_opt(args, "--root")?;
            let skip_if_locked = args.iter().any(|arg| arg == "--skip-if-locked");
            let archive = open_archive(&root)?;
            let report = scrub::scrub(&archive, skip_if_locked)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            print_json(&report);
            if report.ok {
                Ok(0)
            } else {
                Ok(EX_SCRUB)
            }
        }
        "derived-read" => {
            let root = required_opt(args, "--root")?;
            let derived_ref = required_opt(args, "--derived-ref")?;
            let domain = required_opt(args, "--domain")?;
            let out = opt_value(args, "--out")?;
            let archive = open_archive(&root)?;
            let (bytes, meta) = replay::read_derived(&archive, &derived_ref, &domain)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            if let Some(out) = out {
                std::fs::write(&out, &bytes).map_err(|e| (EX_IO, format!("{out}: {e}")))?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "kind": "derived",
                    "derived": meta,
                    "out": out,
                }));
            } else {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(&bytes)
                    .and_then(|_| stdout.flush())
                    .map_err(|e| (EX_IO, e.to_string()))?;
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "kind": "derived",
                        "derived": meta,
                    })
                );
            }
            Ok(0)
        }
        "attach" => {
            let root = required_opt(args, "--root")?;
            let raw = required_opt(args, "--request")?;
            let text = strip_bom(match raw.strip_prefix('@') {
                Some(path) => {
                    std::fs::read_to_string(path).map_err(|e| (EX_USAGE, format!("{path}: {e}")))?
                }
                None => raw,
            });
            let request: AttachmentRequest = serde_json::from_str(&text)
                .map_err(|e| (EX_USAGE, format!("request is not valid JSON: {e}")))?;
            let archive = open_archive(&root)?;
            let report = capture_attachment(&archive, &request)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            print_json(&report);
            if report.ok {
                Ok(0)
            } else {
                let code = if report.status == agent_memory::archive::ATTACHMENT_STATUS_UNAUTHORIZED
                {
                    EX_DENIED
                } else {
                    EX_CAPTURE
                };
                Err((
                    code,
                    report
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("attachment status={}", report.status)),
                ))
            }
        }
        "attachments" => {
            let root = required_opt(args, "--root")?;
            let archive = open_archive(&root)?;
            let entries = archive
                .attachment_entries()
                .map_err(|e| (err_code(&e), e.to_string()))?;
            print_json(&serde_json::json!({
                "ok": true,
                "attachments": entries,
            }));
            Ok(0)
        }
        "attachment-read" => {
            let root = required_opt(args, "--root")?;
            let domain = required_opt(args, "--domain")?;
            let capture_id = required_opt(args, "--capture-id")?;
            let seq: u64 = required_opt(args, "--seq")?
                .parse()
                .map_err(|_| (EX_USAGE, "--seq must be a non-negative integer".to_string()))?;
            let out = opt_value(args, "--out")?;
            let archive = open_archive(&root)?;
            let bytes = archive
                .reader(&domain)
                .read_attachment(&capture_id, seq)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let entry = archive
                .find_attachment(&capture_id, seq)
                .map_err(|e| (err_code(&e), e.to_string()))?
                .map(|(_, entry)| entry)
                .ok_or_else(|| (EX_NOTFOUND, "attachment mapping disappeared".to_string()))?;
            let sha = entry.object_sha256.clone().unwrap_or_default();
            if hex_sha256(&bytes) != sha {
                return Err((
                    EX_NOTFOUND,
                    format!("readback hash mismatch for attachment {capture_id}#{seq}"),
                ));
            }
            let attachment_id = agent_memory::archive::attachment_id(&capture_id, seq);
            if let Some(out) = out {
                std::fs::write(&out, &bytes).map_err(|e| (EX_IO, format!("{out}: {e}")))?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "attachmentId": attachment_id,
                    "sha256": sha,
                    "bytes": bytes.len(),
                    "mediaType": entry.media_type,
                    "verified": true,
                    "out": out,
                }));
            } else {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(&bytes)
                    .and_then(|_| stdout.flush())
                    .map_err(|e| (EX_IO, e.to_string()))?;
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "attachmentId": attachment_id,
                        "sha256": sha,
                        "bytes": bytes.len(),
                        "mediaType": entry.media_type,
                        "verified": true,
                    })
                );
            }
            Ok(0)
        }
        "compact" => {
            // S2.5: migrate plain-encoded objects to the zstd storage encoding
            // in place (content addresses, manifests and records unchanged).
            let root = required_opt(args, "--root")?;
            let dry_run = args.iter().any(|arg| arg == "--dry-run");
            let archive = open_archive(&root)?;
            let report = agent_memory::compact::compact(&archive, dry_run)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            print_json(&report);
            if report.ok {
                Ok(0)
            } else {
                Ok(EX_IO)
            }
        }
        "compact-records" => {
            // implementation-006: rewrite the canonical record ledger from the
            // merged view and drop the journal. The operator command for
            // "compact before a downgrade"; `--dry-run` writes nothing.
            let root = required_opt(args, "--root")?;
            let dry_run = args.iter().any(|arg| arg == "--dry-run");
            let archive = open_archive(&root)?;
            let report = archive
                .compact_records(dry_run)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let mut value = serde_json::to_value(&report)
                .map_err(|e| (EX_IO, format!("compact report: {e}")))?;
            if let Some(map) = value.as_object_mut() {
                map.insert("ok".to_string(), serde_json::Value::Bool(true));
                map.insert(
                    "root".to_string(),
                    serde_json::Value::String(archive.root().to_string_lossy().to_string()),
                );
                map.insert("dryRun".to_string(), serde_json::Value::Bool(dry_run));
            }
            print_json(&value);
            Ok(0)
        }
        "audit-scan" => {
            // S2.5: credential-pattern scan over decoded object bytes for the
            // suite audit (90-audit.ps1). Rules stay defined in
            // agent-memory-common.ps1 and are passed per invocation; the
            // report carries counts only, never matched content.
            let root = required_opt(args, "--root")?;
            let raw = required_opt(args, "--patterns")?;
            let text = strip_bom(match raw.strip_prefix('@') {
                Some(path) => {
                    std::fs::read_to_string(path).map_err(|e| (EX_IO, format!("{path}: {e}")))?
                }
                None => raw,
            });
            let parsed: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| (EX_USAGE, format!("patterns is not valid JSON: {e}")))?;
            let entries = parsed.as_array().ok_or_else(|| {
                (
                    EX_USAGE,
                    "patterns must be a JSON array of {name, pattern}".to_string(),
                )
            })?;
            let mut rules = Vec::new();
            for entry in entries {
                let name = entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| (EX_USAGE, "pattern rule missing name".to_string()))?;
                let pattern = entry
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| (EX_USAGE, format!("pattern rule {name} missing pattern")))?;
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    (
                        EX_USAGE,
                        format!("pattern rule {name} is not a valid regex: {e}"),
                    )
                })?;
                rules.push(agent_memory::audit_scan::ScanRule {
                    name: name.to_string(),
                    regex,
                });
            }
            let archive = open_archive(&root)?;
            let report = agent_memory::audit_scan::scan(&archive, &rules)
                .map_err(|e| (err_code(&e), e.to_string()))?;
            print_json(&report);
            if report.ok {
                Ok(0)
            } else {
                Ok(EX_IO)
            }
        }
        // §E heavy-job gate: may a heavy maintenance job (daily grounding,
        // 7-day verification) run now? Read-only for evidence; the only write is
        // the named `--state` bookkeeping file (deferral count and block start).
        "gate" => {
            let job = opt_value(args, "--job")?.unwrap_or_else(|| "grounding".to_string());
            let state_path = match opt_value(args, "--state")? {
                Some(path) => PathBuf::from(path),
                None => match opt_value(args, "--log-dir")? {
                    Some(dir) => agent_memory::gate::state_path(Path::new(&dir), &job),
                    None => return Err(usage()),
                },
            };
            let idle_minutes = match opt_value(args, "--idle-minutes")? {
                Some(value) => value.parse::<u64>().map_err(|_| usage())?,
                None => agent_memory::gate::IDLE_MIN_MS / 60_000,
            };
            let max_defer_hours = match opt_value(args, "--max-defer-hours")? {
                Some(value) => value.parse::<i64>().map_err(|_| usage())?,
                None => agent_memory::gate::MAX_DEFER_MS / 3_600_000,
            };
            let decision_now = agent_memory::gate::now();
            let mut state = agent_memory::gate::GateState::load(&state_path, &job);
            let observation = agent_memory::gate::observe();
            let policy = agent_memory::gate::GatePolicy {
                idle_min_ms: idle_minutes.saturating_mul(60_000),
                max_defer_ms: max_defer_hours.saturating_mul(3_600_000),
            };
            let decision = agent_memory::gate::assess_job(&observation, &policy, &state, decision_now);
            state.record(&decision, &agent_memory::gate::stamp(decision_now));
            if let Some(parent) = state_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let state_error = state.save(&state_path).err();
            let mut value = serde_json::to_value(&decision).unwrap_or(serde_json::Value::Null);
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("ok".to_string(), serde_json::Value::Bool(true));
                map.insert("job".to_string(), serde_json::Value::String(job.clone()));
                map.insert(
                    "state".to_string(),
                    serde_json::Value::String(state_path.to_string_lossy().to_string()),
                );
                map.insert(
                    "deferrals_total".to_string(),
                    serde_json::Value::from(state.deferrals_total),
                );
                if let Some(error) = state_error {
                    map.insert("state_error".to_string(), serde_json::Value::String(error));
                }
            }
            print_json(&value);
            Ok(0)
        }
        "status" | "reconcile" => {
            let root = required_opt(args, "--root")?;
            let archive = open_archive(&root)?;
            let state = archive
                .load_sources()
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let reconcile = archive
                .reconcile(&[])
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let schema = archive
                .schema()
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let device_id = archive
                .device_id()
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let records = archive
                .load_records()
                .map_err(|e| (err_code(&e), e.to_string()))?;
            let totals = serde_json::json!({
                "sources": state.sources.len(),
                "records": records.records.len(),
                "manifests": reconcile.manifests,
                "objectsOnDisk": reconcile.objects_on_disk,
                "objectsReferenced": reconcile.objects_referenced,
                "orphanObjects": reconcile.orphan_objects.len(),
                "missingObjects": reconcile.missing_objects.len(),
                "stagingResidue": reconcile.staging_residue.len(),
                "attachments": archive
                    .attachment_entries()
                    .map_err(|e| (err_code(&e), e.to_string()))?
                    .len(),
            });
            // P5 status projection: derived checkpoint and last scrub. Both are
            // read-only projections; failures here do not turn `status` into an
            // error (the dedicated commands report them).
            let derived = replay::checkpoint_snapshot(&archive).ok().map(|snapshot| {
                serde_json::json!({
                    "generation": snapshot.generation,
                    "status": snapshot.status,
                    "sources": snapshot.entries.len(),
                    "anomalies": snapshot.anomalies,
                })
            });
            let scrub = scrub::read_ledger(&archive)
                .ok()
                .flatten()
                .map(|ledger| ledger.summary());
            print_json(&serde_json::json!({
                "ok": reconcile.missing_objects.is_empty(),
                "root": archive.root(),
                "deviceId": device_id,
                "schema": schema,
                "lockHeld": archive.lock_held(),
                "sources": state.sources,
                "totals": totals,
                "reconcile": reconcile,
                "derived": derived,
                "scrub": scrub,
            }));
            Ok(0)
        }
        _ => Err(usage()),
    }
}

// keep CaptureReport referenced for the public JSON contract
#[allow(dead_code)]
fn _contract(_: &CaptureReport, _: &PathBuf) {}
