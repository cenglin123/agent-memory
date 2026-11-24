//! Boundary tests: no configuration discovery, no home/XDG reads, no Tauri,
//! `port -> providers` one-way dependency, dependency whitelist.

mod support;

use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::{codes, status};
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::json;
use std::path::{Path, PathBuf};

const WHITELIST: [&str; 10] = [
    "serde",
    "serde_json",
    "log",
    "chrono",
    "toml_edit",
    "regex",
    "zstd",
    "rusqlite",
    "sha2",
    "unicode-normalization",
];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Strip line/doc comments so documentation that *names* a forbidden API does
/// not count as usage. (String literals in this crate do not contain these
/// tokens; the check stays conservative on purpose.)
fn strip_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn recall_sources_have_no_home_or_default_root_channel() {
    let mut files = Vec::new();
    rust_files(&manifest_dir().join("src/recall"), &mut files);
    assert!(!files.is_empty(), "recall sources must exist");

    for path in &files {
        let source = std::fs::read_to_string(path).expect("source");
        let code = strip_comments(&source);
        for banned in [
            "home_dir",
            "dirs::",
            "XDG_",
            "APPDATA",
            "USERPROFILE",
            "env::var",
        ] {
            assert!(
                !code.contains(banned),
                "{} must not contain {banned}",
                path.display()
            );
        }
        // Physical-path resolution is confined to the explicit-root readback
        // helpers; it must never become a general source-discovery mechanism.
        if path.as_path() == manifest_dir().join("src/recall/read.rs").as_path() {
            let narrowed = without_function(
                &without_function(&code, "open_text_at_root"),
                "final_path_of_open_file",
            );
            assert!(
                !narrowed.contains("canonicalize("),
                "{} must confine physical-path resolution to the readback helpers",
                path.display()
            );
        } else {
            assert!(
                !code.contains("canonicalize("),
                "{} must not resolve arbitrary physical paths",
                path.display()
            );
        }
    }
}

/// Drop one top-level function (signature through the closing brace in column
/// zero) so a rule can exempt exactly that helper instead of a whole file.
fn without_function(source: &str, name: &str) -> String {
    let mut kept = Vec::new();
    let mut skipping = false;
    for line in source.lines() {
        if !skipping && line.trim_start().starts_with(&format!("fn {name}")) {
            skipping = true;
        }
        if skipping {
            if line == "}" {
                skipping = false;
            }
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

#[test]
fn port_never_depends_on_providers_or_tauri() {
    let mut port_files = Vec::new();
    rust_files(&manifest_dir().join("src/recall/port"), &mut port_files);
    assert!(!port_files.is_empty());

    for path in &port_files {
        let source = std::fs::read_to_string(path).expect("source");
        let code = strip_comments(&source);
        assert!(
            !code.contains("providers"),
            "{} must not reference providers",
            path.display()
        );
        for banned in ["use base64", "use dirs", "use tauri", "extern crate"] {
            assert!(
                !code.contains(banned),
                "{} must not contain {banned}",
                path.display()
            );
        }
    }

    let mut recall_files = Vec::new();
    rust_files(&manifest_dir().join("src/recall"), &mut recall_files);
    for path in &recall_files {
        let source = std::fs::read_to_string(path).expect("source");
        let code = strip_comments(&source);
        assert!(
            !code.contains("tauri"),
            "{} must not mention tauri in code",
            path.display()
        );
    }
}

#[test]
fn port_uses_only_whitelisted_crates() {
    let mut files = Vec::new();
    rust_files(&manifest_dir().join("src/recall/port"), &mut files);

    // Local modules (declared with `mod x;` anywhere under src/recall) are not
    // third-party crates.
    let mut all_files = Vec::new();
    rust_files(&manifest_dir().join("src/recall"), &mut all_files);
    let mut local_modules: Vec<String> = Vec::new();
    for path in &all_files {
        let source = std::fs::read_to_string(path).expect("source");
        for line in strip_comments(&source).lines() {
            let trimmed = line.trim();
            let spec = trimmed
                .strip_prefix("pub mod ")
                .or_else(|| trimmed.strip_prefix("mod "));
            if let Some(spec) = spec {
                let name = spec.trim_end_matches(';').trim().to_string();
                if !name.is_empty() {
                    local_modules.push(name);
                }
            }
        }
    }

    for path in &files {
        let source = std::fs::read_to_string(path).expect("source");
        for line in strip_comments(&source).lines() {
            let trimmed = line.trim();
            let spec = trimmed
                .strip_prefix("pub use ")
                .or_else(|| trimmed.strip_prefix("use "));
            let Some(spec) = spec else { continue };
            let head = spec
                .split(|ch: char| ch == ':' || ch == ';' || ch == '{' || ch == ' ')
                .next()
                .unwrap_or("")
                .trim();
            if head.is_empty()
                || ["std", "core", "alloc", "crate", "super", "self"].contains(&head)
                || local_modules.iter().any(|module| module == head)
            {
                continue;
            }
            assert!(
                WHITELIST.contains(&head),
                "{} imports non-whitelisted crate {head}",
                path.display()
            );
        }
    }
}

#[test]
fn cargo_dependencies_stay_within_the_whitelist() {
    let manifest = std::fs::read_to_string(manifest_dir().join("Cargo.toml")).expect("Cargo.toml");
    let mut in_dependencies = false;
    let mut seen = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]";
            continue;
        }
        if !in_dependencies || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let name = trimmed.split('=').next().unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue;
        }
        assert!(
            WHITELIST.contains(&name.as_str()),
            "unexpected direct dependency {name}"
        );
        seen.push(name);
    }
    assert!(!seen.is_empty());
    assert!(!seen.iter().any(|name| name == "tauri"));
    assert!(!manifest.contains("tauri"));
    assert!(!manifest.contains("base64"));
    assert!(!manifest.contains("dirs ="));
}

#[test]
fn user_level_decoys_are_never_discovered() {
    let env = support::TestEnv::new();

    // Redirected "home" actually points into the temporary root.
    let home = std::env::var("HOME").expect("HOME redirected");
    assert!(
        home.starts_with(&env.path().to_string_lossy().to_string()),
        "HOME must be redirected, got {home}"
    );
    for key in [
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        let value = std::env::var(key).expect("redirected");
        assert!(value.starts_with(&env.path().to_string_lossy().to_string()));
    }

    // Decoy user-level configuration and agent roots.
    let decoy_source = env.root("decoy-root");
    env.write_json(
        ".config/agent-memory/config.json",
        &env.config_value(
            vec![env.opencode_source("decoy", &decoy_source)],
            vec!["decoy"],
        ),
    );
    env.write_json(".claude/settings.json", &json!({"model": "decoy"}));
    env.write_json(
        ".memory/binding.json",
        &json!({"vault": env.path().to_string_lossy()}),
    );
    env.write(".codex/config.toml", b"model = \"decoy\"");
    env.write(".local/share/opencode/opencode.db", b"decoy database");
    env.write(
        ".claude/projects/decoy/session.jsonl",
        b"{\"text\":\"needlealpha decoy\"}\n",
    );

    // An explicit configuration pointing at an empty source must not fall back
    // to any decoy.
    let empty_root = env.root("explicit-empty");
    let config_path = env.write_json(
        "explicit-config.json",
        &env.config_value(vec![env.opencode_source("oc", &empty_root)], vec!["oc"]),
    );
    let config = load_config(&config_path).expect("explicit config");
    assert_eq!(config.sources.len(), 1);
    assert_eq!(config.sources[0].source_id, "oc");

    let response = memory_search(
        &config,
        &SearchRequest::parse(&json!({"query": "needlealpha"})).unwrap(),
    );
    assert!(
        response.results.is_empty(),
        "decoys must not leak into results"
    );
    assert_eq!(response.source_status.len(), 1);
    assert_eq!(response.source_status[0].source_id, "oc");
    assert_eq!(response.source_status[0].status, status::UNAVAILABLE);
    assert_eq!(response.source_status[0].code, Some(codes::DB_UNAVAILABLE));

    // Without an explicit path there is no discovery: even though a decoy
    // config exists under the redirected home, loading fails closed.
    let error = load_config(Path::new("agent-memory.json")).unwrap_err();
    assert_eq!(error.code, codes::INVALID_REQUEST);
    assert_eq!(error.exit_code(), 2);
}

#[test]
fn every_case_writes_its_own_config() {
    let env = support::TestEnv::new();
    let root = env.root("own-config");
    let first = env.write_json(
        "first.json",
        &env.config_value(vec![env.opencode_source("oc", &root)], vec!["oc"]),
    );
    let second = env.write_json(
        "second.json",
        &env.config_value(vec![env.opencode_source("oc", &root)], vec!["oc"]),
    );
    assert_ne!(first, second);
    let first_config = load_config(&first).unwrap();
    let second_config = load_config(&second).unwrap();
    assert_eq!(first_config, second_config);
    assert_eq!(first_config.source_order, vec!["oc".to_string()]);
}

/// Reverse tripwire: recall reads source bytes and the immutable archive
/// objects only. P1 narrows the historical blanket ban to *write* entry points:
/// an Archive read is legitimate, an Archive write is not. No capture call, no
/// derived replay writer, no rollout writer and no filesystem write API may
/// appear in `src/recall`.
#[test]
fn recall_sources_never_reach_archive_capture_or_write_paths() {
    let mut files = Vec::new();
    rust_files(&manifest_dir().join("src/recall"), &mut files);
    assert!(!files.is_empty());
    for path in &files {
        let source = std::fs::read_to_string(path).expect("source");
        let code = strip_comments(&source);
        for banned in [
            "atomic_write",
            "capture_source",
            "write_rollout",
            "put_object",
            "commit_manifest",
            "save_sources",
            "save_records",
            "acquire_lock",
            "acquire_derived_lock",
            "Archive::init",
            "crate::capture",
            "crate::replay::replay",
            "crate::replay::replay_with_generator",
            "crate::scrub",
            "fs::write",
            "File::create",
            "OpenOptions",
        ] {
            assert!(
                !code.contains(banned),
                "{} must not contain {banned}",
                path.display()
            );
        }
    }
}
