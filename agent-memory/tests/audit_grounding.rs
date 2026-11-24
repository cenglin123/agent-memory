//! Stage 7 §B: `audit-grounding` (derived-text grounding, correction report,
//! body ratio, deterministic sampling, atomic `--out`) and `--conflicts`.
//!
//! Every fixture is synthetic and lives in a fresh temporary directory: no real
//! vault, learning store, archive, session root or recall configuration is
//! touched.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use agent_memory::archive::{hex_sha256, Archive};
use agent_memory::capture::capture_source;
use agent_memory::grounding::{audit_grounding, write_atomic, GroundingOptions};
use agent_memory::learning::LearningStore;

const DOMAIN: &str = "default";
const SESSION_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const SESSION_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn run_grounding(args: &[&str]) -> (i32, Value) {
    let output = cli()
        .arg("audit-grounding")
        .args(args)
        .output()
        .expect("run audit-grounding");
    let code = output.status.code().expect("exit code");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (code, value)
}

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, text).expect("write fixture");
}

/// `vault/.meta/summaries/<rel>` with the documented frontmatter.
fn write_summary(vault: &Path, rel: &str, source: &str, body: &str) {
    let text = format!(
        "---\nsource: {source}\ncontent_hash: 0123456789abcdef\ngenerated_at: \
         2026-11-07T00:00:00Z\nmodel: synthetic\n---\n{body}\n"
    );
    write_file(&vault.join(".meta/summaries").join(rel), &text);
}

fn new_vault() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("vault temp");
    fs::create_dir_all(directory.path().join(".meta/summaries")).expect("summaries");
    fs::create_dir_all(directory.path().join("notes")).expect("notes");
    directory
}

fn items(report: &Value) -> Vec<String> {
    report["pairs"]
        .as_array()
        .expect("pairs")
        .iter()
        .flat_map(|pair| pair["ungrounded"].as_array().expect("ungrounded").iter())
        .map(|item| item["text"].as_str().expect("item text").to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Grounding: exit codes, locations, equivalence, correction report
// ---------------------------------------------------------------------------

#[test]
fn ungrounded_number_version_and_identifiers_exit_one_with_locations() {
    let directory = new_vault();
    let vault = directory.path();
    write_file(
        &vault.join("notes/a.md"),
        "2026-11-07 收录 1129 篇，模型 GLM-5.3，使用 memory_recent。",
    );
    write_summary(
        vault,
        "notes/a.md",
        "notes/a.md",
        "2026-11-08 收录 1130 篇，模型 GLM-5.4，使用 memory_recent_v2。",
    );

    let (code, report) = run_grounding(&["--kb-summaries", &vault.to_string_lossy(), "--json"]);
    assert_eq!(code, 1, "report: {report}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["pairs_checked"], 1);
    assert_eq!(report["pairs_ungrounded"], 1);
    assert_eq!(report["items_ungrounded"], 4, "report: {report}");

    let found = items(&report);
    for expected in ["2026-11-08", "1130 篇", "GLM-5.4", "memory_recent_v2"] {
        assert!(
            found.iter().any(|text| text == expected),
            "missing ungrounded item {expected}: {found:?}"
        );
    }
    let pair = &report["pairs"][0];
    assert_eq!(pair["kind"], "kb_summary");
    assert_eq!(pair["id"], ".meta/summaries/notes/a.md");
    for item in pair["ungrounded"].as_array().expect("ungrounded") {
        assert_eq!(item["line"], 1);
        assert!(item["offset"].as_u64().is_some());
        assert!(!item["context"].as_str().unwrap_or_default().is_empty());
        if item["text"] == "2026-11-08" {
            let candidates = item["candidates"].as_array().expect("candidates");
            assert!(
                candidates
                    .iter()
                    .any(|candidate| candidate["text"].as_str() == Some("2026-11-07")),
                "no plausible correction: {candidates:?}"
            );
        }
    }
    // The correction report repeats the candidates at the top level.
    let dates: Vec<&str> = report["corrections"]
        .as_array()
        .expect("corrections")
        .iter()
        .flat_map(|correction| correction["candidates"].as_array().expect("candidates"))
        .filter_map(|candidate| candidate["text"].as_str())
        .collect();
    assert!(dates.contains(&"2026-11-07") || dates.contains(&"2026年11月7日"));
}

#[test]
fn equivalent_dates_thousands_case_and_fullwidth_are_not_false_positives() {
    let directory = new_vault();
    let vault = directory.path();
    write_file(
        &vault.join("notes/b.md"),
        "2026-11-07 收录 1129 篇，模型 GLM-5.3，使用 memory_recent，版本 v0.3，占 16 MiB。",
    );
    // Every derived spelling is an accepted equivalent of the original.
    write_summary(
        vault,
        "notes/b.md",
        "notes/b.md",
        "２０２６年１１月７日 收录 1,129 篇，模型 ｇｌｍ－５．３，使用 \
         ＭＥＭＯＲＹ＿ＲＥＣＥＮＴ，版本 Ｖ０．３，占 16MiB。",
    );
    // A fourth accepted date spelling in another pair.
    write_file(&vault.join("notes/c.md"), "2026/11/07 发布。");
    write_summary(vault, "notes/c.md", "notes/c.md", "2026-11-7 发布。");

    let (code, report) = run_grounding(&["--kb-summaries", &vault.to_string_lossy()]);
    assert_eq!(code, 0, "report: {report}");
    assert_eq!(report["pairs_checked"], 2);
    assert_eq!(report["items_ungrounded"], 0, "report: {report}");
    assert_eq!(report["ok"], true);
}

#[test]
fn binary_missing_escaping_and_invalid_sources_are_skipped() {
    let directory = new_vault();
    let vault = directory.path();
    write_file(&vault.join("notes/ok.md"), "2026-11-07 正文。");
    write_summary(vault, "notes/ok.md", "notes/ok.md", "2026-11-07 正文。");
    write_file(&vault.join("notes/report.docx"), "not really a docx");
    write_summary(
        vault,
        "notes/report.md",
        "notes/report.docx",
        "2026-11-07 摘要。",
    );
    write_summary(
        vault,
        "notes/missing.md",
        "notes/does-not-exist.md",
        "2026-11-07 摘要。",
    );
    write_summary(vault, "notes/escape.md", "../outside.md", "2026-11-07 摘要。");
    write_file(
        &vault.join(".meta/summaries/notes/invalid.md"),
        "---\ncontent_hash: 0123456789abcdef\n---\nno source\n",
    );

    let (code, report) = run_grounding(&["--kb-summaries", &vault.to_string_lossy()]);
    assert_eq!(code, 0, "report: {report}");
    assert_eq!(report["pairs_checked"], 1);
    assert_eq!(report["skipped"]["binary_source"], 1, "report: {report}");
    assert_eq!(report["skipped"]["source_missing"], 1);
    assert_eq!(report["skipped"]["path_escape"], 1);
    assert_eq!(report["skipped"]["summary_invalid"], 1);
}

#[test]
fn body_ratio_is_reported_per_class() {
    let directory = new_vault();
    let vault = directory.path();
    write_file(&vault.join("notes/long.md"), "2026-11-07 短文。");
    write_summary(
        vault,
        "notes/long.md",
        "notes/long.md",
        "2026-11-07 的摘要比原文长得多，反复补充了背景、上下文、限制条件与后续动作，\
         用来触发单篇派生文本超过原文的计数。",
    );
    write_file(&vault.join("notes/short.md"), "2026-11-07 一段较长的原文，包含若干 \
        背景说明与细节，足以让摘要体量小于原文。");
    write_summary(vault, "notes/short.md", "notes/short.md", "2026-11-07 摘要。");

    let (_, report) = run_grounding(&["--kb-summaries", &vault.to_string_lossy()]);
    let ratio = &report["body_ratio"][0];
    assert_eq!(ratio["kind"], "kb_summary");
    assert_eq!(ratio["pairs"], 2);
    assert_eq!(ratio["single_over_source"], 1, "report: {report}");
    let computed = ratio["derived_bytes"].as_f64().unwrap() / ratio["source_bytes"].as_f64().unwrap();
    assert!((ratio["ratio"].as_f64().unwrap() - computed).abs() < 1e-9);
}

#[test]
fn units_years_counts_and_superscripts_do_not_produce_false_positives() {
    let directory = new_vault();
    let vault = directory.path();
    write_file(
        &vault.join("notes/d.md"),
        "共 385 个样本。2024.2.8 的技术栈。9/22 发布。问题列表。E=mc² 与 x³。",
    );
    write_summary(
        vault,
        "notes/d.md",
        "notes/d.md",
        "约385个样本；2024年的技术栈；9月22日发布；指出了12个方面；E=mc² 与 x³。",
    );

    let (code, report) = run_grounding(&["--kb-summaries", &vault.to_string_lossy()]);
    assert_eq!(code, 0, "no false positive: {report}");
    assert_eq!(report["items_ungrounded"], 0);
    assert_eq!(report["count_checked"], 1, "report: {report}");
    assert_eq!(report["count_ungrounded"], 1);
    assert_eq!(report["pairs"][0]["count_ungrounded"], 1);
    assert_eq!(report["pairs"][0]["count_items"][0]["text"], "12个");
}

#[test]
fn rounding_and_unknown_abbreviations_remain_real_findings() {
    let directory = new_vault();
    let vault = directory.path();
    write_file(
        &vault.join("notes/e.md"),
        "体积 285.52 MB；只使用两种方案。",
    );
    write_summary(
        vault,
        "notes/e.md",
        "notes/e.md",
        "体积从285MB降至100MB；使用 SDS 与 LLM 两种方案。",
    );

    let (code, report) = run_grounding(&["--kb-summaries", &vault.to_string_lossy()]);
    assert_eq!(code, 1, "report: {report}");
    let found = items(&report);
    assert!(found.iter().any(|text| text == "285MB"), "{found:?}");
    assert!(found.iter().any(|text| text == "100MB"), "{found:?}");
    assert!(found.iter().any(|text| text == "SDS"), "{found:?}");
    assert!(found.iter().any(|text| text == "LLM"), "{found:?}");
}

// ---------------------------------------------------------------------------
// Grounding: learning candidates
// ---------------------------------------------------------------------------

struct LearningFixture {
    temp: tempfile::TempDir,
    archive: Archive,
    learning_root: PathBuf,
}

impl LearningFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let archive_root = temp.path().join("archive");
        fs::create_dir_all(&archive_root).expect("archive dir");
        let archive = Archive::init(&archive_root).expect("archive");
        let learning_root = temp.path().join("learning");
        LearningStore::init(&learning_root).expect("learning init");
        Self {
            temp,
            archive,
            learning_root,
        }
    }

    fn capture(&self, content: &str) -> String {
        self.write_and_capture(&[(content, "u1")])
    }

    /// Append one line and capture again: the new record lives in the journal,
    /// the canonical ledger still holds only the earlier one.
    fn capture_appended(&self, first: &str, second: &str) -> (String, String) {
        let first_sha = self.write_and_capture(&[(first, "u1")]);
        let second_sha = self.write_and_capture(&[(first, "u1"), (second, "u2")]);
        (first_sha, second_sha)
    }

    fn write_and_capture(&self, lines: &[(&str, &str)]) -> String {
        let path = self.temp.path().join("source.jsonl");
        let mut body = String::new();
        let mut last = String::new();
        for (content, uuid) in lines {
            let line = json!({
                "type": "user",
                "message": {"role": "user", "content": content},
                "sessionId": "s-grounding",
                "uuid": uuid
            })
            .to_string();
            last = line.clone();
            body.push_str(&line);
            body.push('\n');
        }
        write_file(&path, &body);
        let report =
            capture_source(&self.archive, &path, "claude", DOMAIN, None).expect("capture");
        assert!(report.ok, "capture failed: {:?}", report.error);
        hex_sha256(last.as_bytes())
    }

    fn ingest(&self, claim: &str, applicability: Vec<&str>, shas: &[&str]) {
        let evidence: Vec<Value> = shas
            .iter()
            .map(|sha| {
                json!({
                    "ref": format!("archive-record:{sha}"),
                    "span_sha256": sha,
                    "archive_generation": "1",
                    "observed_at": "2026-11-01T00:00:00Z",
                    "actor": "user",
                    "session_kind": "interactive",
                    "origin_class": "owner",
                    "evidence_kind": "user_statement",
                    "recall_taint": false
                })
            })
            .collect();
        let candidate = json!({
            "schema_version": 1,
            "candidate_id": "",
            "revision": 1,
            "status": "candidate",
            "claim": claim,
            "target_kind": "note",
            "proposed_target": "notes:grounding",
            "applicability": applicability,
            "counterexamples": [],
            "unknowns": [],
            "evidence": evidence,
            "contradicting_evidence": [],
            "dedup": {"query": "", "matches": [], "disposition": "pending", "checked_at": null},
            "reuse": [],
            "validation": [],
            "promotion": {"approval": null, "apply": null, "verification": null},
            "ingestions": [],
            "source_checkpoint": format!("grounding-{}", claim.len()),
            "extractor": "grounding-test",
            "created_at": "2026-11-01T00:00:00Z",
            "updated_at": "2026-11-01T00:00:00Z"
        });
        let path = self.temp.path().join("candidate.json");
        write_file(&path, &candidate.to_string());
        let output = cli()
            .arg("learning")
            .arg("ingest")
            .arg("--root")
            .arg(&self.learning_root)
            .arg("--candidate")
            .arg(format!("@{}", path.display()))
            .output()
            .expect("ingest");
        assert!(
            output.status.success(),
            "ingest failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn learning_claim_grounds_against_a_record_that_only_lives_in_the_journal() {
    // The grounding pair reads the committed record index through the merged
    // ledger view (implementation-006): a record appended after the last
    // compaction resolves without one.
    let fixture = LearningFixture::new();
    let (_, second) = fixture.capture_appended(
        "2026-11-07 收录 1129 篇。",
        "2026-11-08 收录 1130 篇。",
    );
    let canonical = fs::read_to_string(
        fixture.temp.path().join("archive").join("state").join("records.json"),
    )
    .expect("canonical ledger");
    assert!(
        !canonical.contains(&second),
        "the fixture record must live in the journal only"
    );

    fixture.ingest("2026-11-08 收录 1130 篇", vec![], &[&second]);
    let (code, report) = run_grounding(&[
        "--learning-root",
        &fixture.learning_root.to_string_lossy(),
        "--archive",
        &fixture.temp.path().join("archive").to_string_lossy(),
        "--domain",
        DOMAIN,
    ]);
    assert_eq!(code, 0, "report: {report}");
    assert_eq!(report["pairs_checked"], 1, "report: {report}");
    assert_eq!(report["items_ungrounded"], 0, "report: {report}");
    assert_eq!(
        report["skipped"]["ref_unreadable"].as_u64().unwrap_or(0),
        0,
        "the journal-only record is readable: {report}"
    );
}

#[test]
fn learning_claim_grounds_against_archive_evidence_and_applicability_is_separate() {
    let fixture = LearningFixture::new();
    let sha = fixture.capture("2026-11-07 收录 1129 篇，模型 GLM-5.3。");
    fixture.ingest(
        "2026-11-07 收录 1129 篇",
        vec!["仅在 2026-11-08 的版本上"],
        &[&sha],
    );
    let missing = "f".repeat(64);
    fixture.ingest("second claim", vec![], &[&missing]);

    // Without an archive the whole learning class is skipped, not failed.
    let (code, report) = run_grounding(&[
        "--learning-root",
        &fixture.learning_root.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "report: {report}");
    assert_eq!(report["pairs_checked"], 0);
    assert_eq!(report["skipped"]["archive_not_supplied"], 2);

    let (code, report) = run_grounding(&[
        "--learning-root",
        &fixture.learning_root.to_string_lossy(),
        "--archive",
        &fixture.temp.path().join("archive").to_string_lossy(),
        "--domain",
        DOMAIN,
    ]);
    assert_eq!(code, 0, "applicability never affects the exit code: {report}");
    assert_eq!(report["pairs_checked"], 1);
    assert_eq!(report["items_checked"], 2, "report: {report}");
    assert_eq!(report["items_ungrounded"], 0);
    assert_eq!(report["applicability_checked"], 1);
    assert_eq!(report["applicability_ungrounded"], 1);
    assert_eq!(report["skipped"]["ref_unreadable"], 1);
    let pair = &report["pairs"][0];
    assert_eq!(pair["kind"], "learning_candidate");
    assert_eq!(pair["applicability"]["items_ungrounded"], 1);
    assert_eq!(
        pair["applicability"]["ungrounded"][0]["text"],
        "2026-11-08"
    );
}

#[test]
fn learning_input_is_byte_identical_after_the_run() {
    let fixture = LearningFixture::new();
    let sha = fixture.capture("2026-11-07 收录 1129 篇。");
    fixture.ingest("2026-11-07 收录 1129 篇", vec![], &[&sha]);
    // A knowledge-base summary pair is read too: the vault must not change.
    let vault = fixture.temp.path().join("vault");
    fs::create_dir_all(vault.join(".meta/summaries/notes")).expect("summaries");
    fs::create_dir_all(vault.join("notes")).expect("notes");
    write_file(&vault.join("notes/a.md"), "2026-11-07 收录 1129 篇。");
    write_summary(&vault, "notes/a.md", "notes/a.md", "2026-11-07 收录 1129 篇。");

    let before = tree_digest(fixture.temp.path());
    let options = GroundingOptions {
        kb_summaries: Some(vault),
        learning_root: Some(fixture.learning_root.clone()),
        archive: Some(
            fixture
                .temp
                .path()
                .join("archive")
                .to_string_lossy()
                .into_owned(),
        ),
        domain: Some(DOMAIN.to_string()),
        ..GroundingOptions::default()
    };
    let report = audit_grounding(&options).expect("grounding");
    assert_eq!(report.pairs_checked, 2, "report: {report:?}");
    let after = tree_digest(fixture.temp.path());
    // SQLite creates its WAL sidecars when a read-only connection reads a WAL
    // database; no pre-existing file may change or disappear.
    for (name, digest) in &before {
        assert_eq!(
            after.get(name),
            Some(digest),
            "grounding modified its input file {name}"
        );
    }
    assert!(after.contains_key("learning/learning.db"));
}

fn tree_digest(root: &Path) -> BTreeMap<String, (u64, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, (u64, String)>) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let bytes = fs::read(&path).expect("read fixture");
                let name = path
                    .strip_prefix(base)
                    .expect("prefix")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(name, (bytes.len() as u64, hex_sha256(&bytes)));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Sampling and atomic output
// ---------------------------------------------------------------------------

#[test]
fn sample_and_seed_are_reproducible() {
    let directory = new_vault();
    let vault = directory.path();
    for index in 0..6 {
        let name = format!("notes/{index}.md");
        write_file(&vault.join(&name), "2026-11-07 原文。");
        write_summary(vault, &name, &name, "2026-11-07 摘要。");
    }
    let vault_text = vault.to_string_lossy().into_owned();
    let args = [
        "--kb-summaries",
        vault_text.as_str(),
        "--sample",
        "3",
        "--seed",
        "42",
    ];
    let (first_code, first) = run_grounding(&args);
    let (second_code, second) = run_grounding(&args);
    assert_eq!(first_code, 0);
    assert_eq!(second_code, 0);
    assert_eq!(first["sample"]["size"], 3);
    assert_eq!(first["sample"]["seed"], 42);
    assert_eq!(first["sample"]["pairs_total"], 6);
    assert_eq!(first["sample"]["pairs_selected"], 3);
    assert_eq!(first["pairs"].as_array().unwrap().len(), 3);
    let ids = |report: &Value| -> Vec<String> {
        report["pairs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| pair["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&first), ids(&second));
}

#[test]
fn out_is_written_atomically_and_replaces_the_target() {
    let directory = new_vault();
    let vault = directory.path();
    write_file(&vault.join("notes/a.md"), "2026-11-07 原文。");
    write_summary(vault, "notes/a.md", "notes/a.md", "2026-11-08 摘要。");
    let out = directory.path().join("grounding-last.json");
    fs::write(&out, b"stale bytes that must be replaced").expect("seed out");

    let (code, stdout) = run_grounding(&[
        "--kb-summaries",
        &vault.to_string_lossy(),
        "--out",
        &out.to_string_lossy(),
    ]);
    assert_eq!(code, 1);
    let written: Value =
        serde_json::from_slice(&fs::read(&out).expect("read out")).expect("out JSON");
    assert_eq!(written, stdout);
    let leftovers = fs::read_dir(directory.path())
        .expect("dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        .count();
    assert_eq!(leftovers, 0, "temporary file was left behind");
}

#[test]
fn write_atomic_refuses_a_missing_directory() {
    let directory = tempfile::tempdir().expect("temp");
    let path = directory.path().join("missing").join("report.json");
    let error = write_atomic(&path, b"{}").expect_err("must refuse");
    assert!(error.message.contains("does not exist"), "{error}");
}

// ---------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------

fn claude_line(session: &str, uuid: &str, time: &str, kind: &str, text: &str) -> String {
    json!({
        "type": kind,
        "message": {"role": kind, "content": text},
        "timestamp": time,
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

struct ConflictFixture {
    temp: tempfile::TempDir,
    config: PathBuf,
}

fn conflict_fixture() -> ConflictFixture {
    let a = vec![claude_line(
        SESSION_A,
        "a1",
        "2026-08-01T10:00:00Z",
        "user",
        "本地默认用 qwen3.5:9b 跑嵌入",
    )];
    let b = vec![
        claude_line(
            SESSION_B,
            "b1",
            "2026-11-01T10:00:00Z",
            "user",
            "不要用 qwen3.5:9b 了",
        ),
        // Every exclusion class, all with the same identifier.
        claude_line(SESSION_B, "b2", "2026-11-01T10:01:00Z", "user", "要不要用 qwen3.5:9b？"),
        claude_line(SESSION_B, "b3", "2026-11-01T10:02:00Z", "user", "用 qwen3.5:9b 吗"),
        claude_line(SESSION_B, "b4", "2026-11-01T10:03:00Z", "user", "如果要用 qwen3.5:9b 就先测"),
        claude_line(SESSION_B, "b5", "2026-11-01T10:04:00Z", "user", "比如用 qwen3.5:9b"),
        claude_line(SESSION_B, "b6", "2026-11-01T10:05:00Z", "user", "他说不要用 qwen3.5:9b"),
        claude_line(SESSION_B, "b7", "2026-11-01T10:06:00Z", "user", "「不要用 qwen3.5:9b」"),
        // Non-usage verbs and non-anchor words never pair.
        claude_line(SESSION_B, "b8", "2026-11-01T10:07:00Z", "user", "不要改 sync_agents.py 的实现"),
        claude_line(SESSION_B, "b9", "2026-11-01T10:08:00Z", "user", "不要动 qwen3.5:9b 的配置"),
        claude_line(SESSION_B, "b10", "2026-11-01T10:09:00Z", "user", "用 Glob 搜一下"),
        // Assistant text is never a conflict side.
        claude_line(
            SESSION_B,
            "b12",
            "2026-11-01T10:11:00Z",
            "assistant",
            "我建议不要用 qwen3.5:9b",
        ),
    ];
    // A dispatched worker session: its user text is a task prompt, not an
    // owner statement.
    let worker_session = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let worker = vec![
        claude_line(
            worker_session,
            "w1",
            "2026-11-02T10:00:00Z",
            "user",
            "禁止使用 qwen3.5:9b",
        ),
        claude_line(
            worker_session,
            "w2",
            "2026-11-02T10:01:00Z",
            "user",
            "不要使用 qwen3.5:9b",
        ),
    ];
    build_conflict_fixture(&[
        (
            format!("proj/{SESSION_A}.jsonl"),
            a,
        ),
        (
            format!("proj/{SESSION_B}.jsonl"),
            b,
        ),
        (
            format!("proj/subagents/{worker_session}.jsonl"),
            worker,
        ),
    ])
}

fn build_conflict_fixture(files: &[(String, Vec<String>)]) -> ConflictFixture {
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path().join("claude");
    fs::create_dir_all(&root).expect("project");
    for (rel, lines) in files {
        write_file(&root.join(rel), &(lines.join("\n") + "\n"));
    }
    let cache = temp.path().join("cache");
    let config = temp.path().join("recall.json");
    write_file(
        &config,
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
        })
        .to_string(),
    );
    let output = cli()
        .arg("index")
        .arg("--config")
        .arg(&config)
        .output()
        .expect("index");
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    ConflictFixture { temp, config }
}

#[test]
fn conflicts_pair_opposite_user_statements_and_ignore_every_exclusion() {
    let fixture = conflict_fixture();
    let (code, report) = run_grounding(&[
        "--conflicts",
        "--config",
        &fixture.config.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "conflicts are needs_review, never a failure: {report}");
    let conflicts = report["conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1, "report: {report}");
    let pair = &conflicts[0];
    assert_eq!(pair["status"], "needs_review");
    assert_eq!(pair["affirmative"]["polarity"], "affirmative");
    assert_eq!(pair["negative"]["polarity"], "negative");
    assert_eq!(pair["affirmative"]["session_id"], SESSION_A);
    assert_eq!(pair["negative"]["session_id"], SESSION_B);
    assert_eq!(pair["affirmative_count"], 1);
    assert_eq!(pair["negative_count"], 1);
    assert_eq!(pair["affirmative"]["time"], "2026-08-01T10:00:00.000Z");
    assert_eq!(pair["negative"]["time"], "2026-11-01T10:00:00.000Z");
    assert!(pair["affirmative"]["snippet"]
        .as_str()
        .unwrap()
        .contains("qwen3.5:9b"));
    assert_eq!(report["conflict_sources"][0]["status"], "scanned");
    // Only interactive user text units are read: one from A plus ten from B.
    // The worker session (two user prompts) is filtered out before pairing.
    assert_eq!(report["conflict_sources"][0]["rows_examined"], 11);
    let excluded = &report["conflict_excluded"];
    for class in [
        "negated_question",
        "question",
        "conditional",
        "example",
        "quotation",
        "third_party",
    ] {
        assert!(
            excluded[class].as_u64().unwrap_or(0) >= 1,
            "exclusion {class} did not fire: {excluded}"
        );
    }

    // Both refs read back through `memory_read`.
    for side in ["affirmative", "negative"] {
        let request = json!({"ref": pair[side]["ref"], "max_bytes": 4096});
        let path = fixture.temp.path().join(format!("read-{side}.json"));
        write_file(&path, &request.to_string());
        let output = cli()
            .arg("memory_read")
            .arg("--config")
            .arg(&fixture.config)
            .arg("--request")
            .arg(format!("@{}", path.display()))
            .output()
            .expect("memory_read");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{side} ref is not readable: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let read: Value = serde_json::from_slice(&output.stdout).expect("read JSON");
        assert_eq!(read["ok"], true, "readback failed: {read}");
        let text = read["segments"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("qwen3.5:9b"), "unexpected readback text: {text}");
    }

    // `--since` after the affirmative leaves nothing to pair.
    let (_, since_report) = run_grounding(&[
        "--conflicts",
        "--config",
        &fixture.config.to_string_lossy(),
        "--since",
        "2026-10-01T00:00:00Z",
    ]);
    assert!(since_report["conflicts"].as_array().unwrap().is_empty());
}

#[test]
fn conflicts_converge_to_one_group_per_identifier_with_counts() {
    let affirmative_session = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let negative_session = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let mut affirmatives = Vec::new();
    for index in 0..10 {
        affirmatives.push(claude_line(
            affirmative_session,
            &format!("d{index}"),
            &format!("2026-09-01T00:{index:02}:00Z"),
            "user",
            "默认用 qwen3.5:9b",
        ));
    }
    let mut negatives = Vec::new();
    for index in 0..3 {
        negatives.push(claude_line(
            negative_session,
            &format!("e{index}"),
            &format!("2026-10-01T00:{index:02}:00Z"),
            "user",
            "不要使用 qwen3.5:9b",
        ));
    }
    let fixture = build_conflict_fixture(&[
        (
            format!("proj/{affirmative_session}.jsonl"),
            affirmatives,
        ),
        (format!("proj/{negative_session}.jsonl"), negatives),
    ]);
    let (code, report) = run_grounding(&[
        "--conflicts",
        "--config",
        &fixture.config.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{report}");
    let conflicts = report["conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1, "one group per identifier: {report}");
    let pair = &conflicts[0];
    assert_eq!(pair["affirmative_count"], 10);
    assert_eq!(pair["negative_count"], 3);
    assert_eq!(pair["affirmative"]["time"], "2026-09-01T00:09:00.000Z");
    assert_eq!(pair["negative"]["time"], "2026-10-01T00:02:00.000Z");
    assert_eq!(pair["affirmative"]["session_id"], affirmative_session);
    assert_eq!(pair["negative"]["session_id"], negative_session);
}

#[test]
fn conflicts_require_a_config_and_a_config_requires_conflicts() {
    let output = cli()
        .arg("audit-grounding")
        .arg("--conflicts")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));

    let output = cli()
        .arg("audit-grounding")
        .arg("--kb-summaries")
        .arg("C:/does/not/matter")
        .arg("--config")
        .arg("C:/does/not/matter.json")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
}

/// A mid-turn message as Claude Code stores it: an `attachment` record, not a
/// `type: "user"` record.
fn claude_queued(session: &str, uuid: &str, time: &str, text: &str, origin: &str) -> String {
    json!({
        "type": "attachment",
        "sessionId": session,
        "uuid": uuid,
        "timestamp": time,
        "isSidechain": false,
        "attachment": {
            "type": "queued_command",
            "prompt": text,
            "source_uuid": "source-1",
            "delivery_id": "delivery-1",
            "commandMode": "prompt",
            "origin": {"kind": origin},
            "timestamp": time,
            "humanTurn": origin == "human"
        },
        "rendered": [],
        "renderedRole": "system"
    })
    .to_string()
}

#[test]
fn conflicts_read_midturn_prompts_but_never_peer_messages() {
    let peer_session = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let fixture = build_conflict_fixture(&[
        (
            format!("proj/{SESSION_A}.jsonl"),
            vec![claude_line(
                SESSION_A,
                "a1",
                "2026-08-01T10:00:00Z",
                "user",
                "本地默认用 qwen3.5:9b 跑嵌入",
            )],
        ),
        (
            format!("proj/{SESSION_B}.jsonl"),
            vec![
                // The owner's mid-turn correction is user text.
                claude_queued(
                    SESSION_B,
                    "b1",
                    "2026-11-01T10:00:00Z",
                    "不要用 qwen3.5:9b 了",
                    "human",
                ),
                // Bookkeeping copies of the same text are not.
                json!({
                    "type": "queue-operation",
                    "operation": "enqueue",
                    "sessionId": SESSION_B,
                    "uuid": "b2",
                    "timestamp": "2026-11-01T10:00:01Z",
                    "content": "不要用 qwen3.5:9b 了"
                })
                .to_string(),
                json!({
                    "type": "api-request-blob",
                    "sessionId": SESSION_B,
                    "uuid": "b3",
                    "timestamp": "2026-11-01T10:00:02Z",
                    "request": {"messages": [{"role": "user", "content": "不要用 qwen3.5:9b 了"}]}
                })
                .to_string(),
            ],
        ),
        // Another session's SendMessage is injected context, never the owner.
        (
            format!("proj/{peer_session}.jsonl"),
            vec![claude_queued(
                peer_session,
                "c1",
                "2026-11-02T10:00:00Z",
                "不要使用 qwen3.5:9b",
                "peer",
            )],
        ),
    ]);

    let (code, report) = run_grounding(&[
        "--conflicts",
        "--config",
        &fixture.config.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{report}");
    let conflicts = report["conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1, "one group: {report}");
    let pair = &conflicts[0];
    assert_eq!(pair["affirmative"]["session_id"], SESSION_A);
    assert_eq!(pair["negative"]["session_id"], SESSION_B);
    assert_eq!(pair["negative_count"], 1, "the peer message is not a side");
    assert_eq!(pair["affirmative_count"], 1);
    assert_eq!(
        report["conflict_sources"][0]["rows_examined"], 2,
        "one user text per session; peer/notification copies are not units: {report}"
    );

    // The negative ref reads back the mid-turn prompt.
    let request = json!({"ref": pair["negative"]["ref"], "max_bytes": 4096});
    let path = fixture.temp.path().join("read-midturn.json");
    write_file(&path, &request.to_string());
    let output = cli()
        .arg("memory_read")
        .arg("--config")
        .arg(&fixture.config)
        .arg("--request")
        .arg(format!("@{}", path.display()))
        .output()
        .expect("memory_read");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let read: Value = serde_json::from_slice(&output.stdout).expect("read JSON");
    assert_eq!(read["ok"], true, "{read}");
    assert_eq!(
        read["segments"][0]["text"].as_str().unwrap_or_default().trim(),
        "不要用 qwen3.5:9b 了"
    );
    assert_eq!(
        pair["negative"]["ref"]["locator"]["pointer"],
        json!("/attachment/prompt")
    );
}
