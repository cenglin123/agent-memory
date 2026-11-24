//! The single explicit configuration source (§3.0).
//!
//! There is no discovery: the caller passes one file path. No `$HOME`,
//! `USERPROFILE`, `%APPDATA%`, `%LOCALAPPDATA%`, XDG directory, binding file or
//! environment variable is ever consulted, and there is no default value for
//! `interpreter.kb.command`.
//!
//! Configuration version 2 is the only writable shape. Version 1 is accepted
//! and normalized **in memory only** (`domain = "default"`, no archive source,
//! no telemetry); the file is never rewritten by this crate.

use crate::recall::contract::RecallError;
use crate::recall::domain::{parse_domain_value, DEFAULT_DOMAIN};
use crate::recall::semantic::model::EmbeddingModelSpec;
use crate::recall::telemetry::TelemetryConfig;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const CONFIG_VERSION: u32 = 2;
pub const CONFIG_VERSION_V1: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Kb,
    Device,
    Docs,
    Opencode,
    Codex,
    ClaudeCode,
    Dsh,
    Archive,
    Hermes,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::Kb => "kb",
            SourceKind::Device => "device",
            SourceKind::Docs => "docs",
            SourceKind::Opencode => "opencode",
            SourceKind::Codex => "codex",
            SourceKind::ClaudeCode => "claude_code",
            SourceKind::Dsh => "dsh",
            SourceKind::Archive => "archive",
            SourceKind::Hermes => "hermes",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "kb" => Some(SourceKind::Kb),
            "device" => Some(SourceKind::Device),
            "docs" => Some(SourceKind::Docs),
            "opencode" => Some(SourceKind::Opencode),
            "codex" => Some(SourceKind::Codex),
            "claude_code" => Some(SourceKind::ClaudeCode),
            "dsh" => Some(SourceKind::Dsh),
            "archive" => Some(SourceKind::Archive),
            "hermes" => Some(SourceKind::Hermes),
            _ => None,
        }
    }

    /// A live source the full-text index can be built for. KB keeps `ask.py`
    /// and Archive keeps its replay generations; neither is indexed here.
    pub fn is_indexable(&self) -> bool {
        matches!(
            self,
            SourceKind::Opencode
                | SourceKind::Codex
                | SourceKind::ClaudeCode
                | SourceKind::Dsh
                | SourceKind::Hermes
                | SourceKind::Device
                | SourceKind::Docs
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    pub source_id: String,
    pub kind: SourceKind,
    /// The configuration string verbatim. Root hashing and every runtime
    /// comparison use this exact value; `canonicalize()` is never applied.
    pub root: String,
    pub enabled: bool,
    pub db: Option<String>,
    pub storage: Option<String>,
    /// `docs` only: file globs, relative to the source root. Empty means the
    /// default (`**/*.md`). Other kinds reject these keys.
    pub include: Vec<String>,
    /// `docs` only: paths skipped after `include`.
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbInterpreter {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterConfig {
    pub kb: KbInterpreter,
    /// Present only in a v2 configuration. P3 owns the embedding execution;
    /// P0 freezes the model identity that enters the retrieval generation key.
    pub embedding: Option<EmbeddingModelSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallConfig {
    /// The version read from the file: `1` (normalized in memory) or `2`.
    pub version: u32,
    pub sources: Vec<SourceConfig>,
    pub source_order: Vec<String>,
    pub interpreter: InterpreterConfig,
    /// Effective configuration-layer domain. Always `"default"` for v1.
    pub domain: String,
    /// `None` for v1 and for a v2 file without the optional block.
    pub telemetry: Option<TelemetryConfig>,
    /// Explicit root of the deletable live-source full-text cache. Indexes live
    /// at `<cache_root>/live/<source_id>/fts.sqlite`. Never derived from a home
    /// directory: `index` refuses to run when the key is absent.
    pub cache_root: Option<String>,
    /// Sources whose `kind` this binary does not know, skipped at load time
    /// (forward compatibility with configurations written by newer installers).
    pub unsupported_sources: Vec<UnsupportedSource>,
    /// Unknown **top-level** keys of a v2 configuration, in sorted order. They
    /// are ignored instead of rejecting the whole file, so an older process
    /// that re-reads a configuration written by a newer installer keeps
    /// serving; the response discloses them as `WARN_CONFIG_KEY_UNSUPPORTED`.
    ///
    /// A spelling variant of a known key (equal after ASCII case folding and
    /// `-`/`_` folding, such as `cache-root` or `Source_Order`) is **not**
    /// ignored: it still fails as `unknown_key` so a typo is never silent.
    /// Version 1 keeps the original strict behavior (this list stays empty).
    pub unsupported_keys: Vec<String>,
}

/// A configured source this binary cannot serve: its `kind` is unknown here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedSource {
    pub source_id: String,
    pub kind: String,
    /// A disabled entry is skipped silently: it would not be served anyway.
    pub enabled: bool,
}

impl RecallConfig {
    /// One `WARN_SOURCE_KIND_UNSUPPORTED` per skipped source, for responses.
    pub fn unsupported_source_warnings(
        &self,
        scope: &'static str,
    ) -> Vec<crate::recall::contract::WarningEntry> {
        self.unsupported_sources
            .iter()
            .filter(|skipped| skipped.enabled)
            .map(|skipped| crate::recall::contract::WarningEntry {
                scope,
                source_id: Some(skipped.source_id.clone()),
                hit_id: None,
                code: crate::recall::contract::WARN_SOURCE_KIND_UNSUPPORTED,
                detail: Some(serde_json::json!({
                    "kind": skipped.kind,
                    "reason": "kind_unknown_to_this_binary",
                })),
            })
            .collect()
    }

    /// One `WARN_CONFIG_KEY_UNSUPPORTED` per response when a v2 configuration
    /// carries top-level keys this binary does not know. All keys go into one
    /// `detail.keys` array so a single warning names the whole set.
    pub fn unsupported_key_warnings(
        &self,
        scope: &'static str,
    ) -> Vec<crate::recall::contract::WarningEntry> {
        if self.unsupported_keys.is_empty() {
            return Vec::new();
        }
        vec![crate::recall::contract::WarningEntry {
            scope,
            source_id: None,
            hit_id: None,
            code: crate::recall::contract::WARN_CONFIG_KEY_UNSUPPORTED,
            detail: Some(serde_json::json!({ "keys": self.unsupported_keys })),
        }]
    }

    pub fn source(&self, source_id: &str) -> Option<&SourceConfig> {
        self.sources.iter().find(|s| s.source_id == source_id)
    }

    /// `enabled = true` source ids, in configuration order.
    pub fn enabled_sources(&self) -> Vec<String> {
        self.sources
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.source_id.clone())
            .collect()
    }

    /// Position in `source_order` (all requested-but-unconfigured ids miss).
    pub fn source_order_index(&self, source_id: &str) -> Option<usize> {
        self.source_order.iter().position(|id| id == source_id)
    }

    /// `request > configuration > default` (§3.2). The returned value is the
    /// effective access domain; it never widens an Archive access label set.
    pub fn effective_domain(&self, requested: Option<&str>) -> String {
        requested
            .map(|value| value.to_string())
            .unwrap_or_else(|| self.domain.clone())
    }

    /// True when the domain was selected by the request or by a non-default
    /// configuration value. A bare `default` (including v1 normalization) is
    /// an implicit selection, so non-default Archive labels stay unreachable.
    pub fn domain_is_explicit(&self, requested: Option<&str>) -> bool {
        requested.is_some() || self.domain != DEFAULT_DOMAIN
    }

    pub fn is_v2(&self) -> bool {
        self.version == CONFIG_VERSION
    }

    pub fn telemetry_enabled(&self) -> bool {
        self.telemetry
            .as_ref()
            .map(|telemetry| telemetry.enabled)
            .unwrap_or(false)
    }
}

/// Read + validate an explicit configuration file.
pub fn load_config(path: &Path) -> Result<RecallConfig, RecallError> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        RecallError::invalid(
            "config",
            "config_unreadable",
            format!("configuration file cannot be read: {err}"),
        )
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|err| {
        RecallError::invalid(
            "config",
            "config_invalid_json",
            format!("configuration file is not valid JSON: {err}"),
        )
    })?;
    RecallConfig::from_value(&value)
}

impl RecallConfig {
    pub fn from_value(value: &Value) -> Result<RecallConfig, RecallError> {
        let top = as_object(value, "config")?;
        let version_value = require(top, "version", "")?;
        let version_number = match version_value {
            Value::Number(number) => number
                .as_u64()
                .ok_or_else(|| invalid_type("version", "integer"))?,
            _ => return Err(invalid_type("version", "integer")),
        };
        if version_number != CONFIG_VERSION as u64
            && version_number != CONFIG_VERSION_V1 as u64
        {
            return Err(RecallError::invalid(
                "version",
                "unsupported_version",
                format!(
                    "configuration version must be {CONFIG_VERSION_V1} or {CONFIG_VERSION}"
                ),
            ));
        }
        let version = version_number as u32;
        let v2 = version == CONFIG_VERSION;

        // A top-level key this binary does not know is ignored and disclosed
        // for a v2 file (an older process keeps serving a configuration written
        // by a newer installer). A spelling variant of a known key is still an
        // error, and v1 stays strict.
        const V1_TOP_KEYS: [&str; 4] = ["version", "sources", "source_order", "interpreter"];
        const V2_TOP_KEYS: [&str; 7] = [
            "version",
            "sources",
            "source_order",
            "interpreter",
            "domain",
            "telemetry",
            "cache_root",
        ];
        let mut unsupported_keys: Vec<String> = Vec::new();
        if v2 {
            for key in top.keys() {
                if V2_TOP_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if V2_TOP_KEYS
                    .iter()
                    .any(|known| fold_config_key(known) == fold_config_key(key))
                {
                    return Err(RecallError::invalid(
                        key,
                        "unknown_key",
                        format!("unknown configuration key: {key}"),
                    ));
                }
                unsupported_keys.push(key.clone());
            }
            unsupported_keys.sort();
        } else {
            reject_unknown(top, &V1_TOP_KEYS, "")?;
        }

        let sources_value = require(top, "sources", "")?;
        let sources_array = sources_value
            .as_array()
            .ok_or_else(|| invalid_type("sources", "array"))?;
        if sources_array.is_empty() {
            return Err(RecallError::invalid(
                "sources",
                "empty_sources",
                "sources[] must contain at least one source",
            ));
        }
        let mut sources = Vec::with_capacity(sources_array.len());
        let mut unsupported_sources: Vec<UnsupportedSource> = Vec::new();
        let mut seen = BTreeSet::new();
        for (index, raw) in sources_array.iter().enumerate() {
            // A well-formed entry whose `kind` this binary does not know was
            // written by a newer installer: skip it (and its `source_order`
            // entry) instead of rejecting the whole configuration, so an older
            // process that is still running keeps serving every other source.
            if let Some(skipped) = unknown_kind_source(raw) {
                if !seen.insert(skipped.source_id.clone()) {
                    return Err(RecallError::invalid(
                        &format!("sources[{index}].source_id"),
                        "duplicate_source_id",
                        "source_id must be unique within the configuration",
                    ));
                }
                unsupported_sources.push(skipped);
                continue;
            }
            let source = parse_source(raw, index, v2)?;
            if !seen.insert(source.source_id.clone()) {
                return Err(RecallError::invalid(
                    &format!("sources[{index}].source_id"),
                    "duplicate_source_id",
                    "source_id must be unique within the configuration",
                ));
            }
            sources.push(source);
        }

        if sources.is_empty() {
            // Nothing this binary can serve: report the first unknown kind
            // exactly as before rather than a vaguer "no enabled source".
            if let Some((index, _)) = sources_array
                .iter()
                .enumerate()
                .find(|(_, raw)| unknown_kind_source(raw).is_some())
            {
                return Err(RecallError::invalid(
                    &format!("sources[{index}].kind"),
                    "unsupported_kind",
                    "kind must be one of kb, device, docs, opencode, codex, claude_code, dsh, hermes, archive",
                ));
            }
        }

        let order_value = require(top, "source_order", "")?;
        let order_array = order_value
            .as_array()
            .ok_or_else(|| invalid_type("source_order", "array"))?;
        let mut source_order = Vec::with_capacity(order_array.len());
        let mut order_seen = BTreeSet::new();
        for (index, raw) in order_array.iter().enumerate() {
            let id = raw
                .as_str()
                .ok_or_else(|| invalid_type(&format!("source_order[{index}]"), "string"))?;
            if !order_seen.insert(id.to_string()) {
                return Err(RecallError::invalid(
                    &format!("source_order[{index}]"),
                    "duplicate_order_entry",
                    "source_order must not repeat a source_id",
                ));
            }
            if unsupported_sources.iter().any(|skipped| skipped.source_id == id) {
                continue;
            }
            source_order.push(id.to_string());
        }

        let enabled: BTreeSet<String> = sources
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.source_id.clone())
            .collect();
        if enabled.is_empty() {
            return Err(RecallError::invalid(
                "sources",
                "no_enabled_source",
                "at least one source must be enabled",
            ));
        }
        let ordered: BTreeSet<String> = source_order.iter().cloned().collect();
        if ordered != enabled || source_order.len() != enabled.len() {
            let missing: Vec<&String> = enabled.difference(&ordered).collect();
            let extra: Vec<&String> = ordered.difference(&enabled).collect();
            return Err(RecallError::invalid(
                "source_order",
                "source_order_mismatch",
                format!(
                    "source_order must exactly cover the enabled source ids (missing: {:?}, unexpected: {:?})",
                    missing, extra
                ),
            ));
        }

        let interpreter_value = require(top, "interpreter", "")?;
        let interpreter = parse_interpreter(interpreter_value, v2)?;

        let domain = if v2 {
            parse_domain_value(require(top, "domain", "")?, "domain")?
        } else {
            // v1 is normalized in memory; the file is never rewritten.
            DEFAULT_DOMAIN.to_string()
        };
        let telemetry = if v2 {
            match top.get("telemetry") {
                None => None,
                Some(value) => Some(parse_telemetry(value)?),
            }
        } else {
            None
        };
        let cache_root = if v2 {
            match optional_path(top, "cache_root", "")? {
                None => None,
                Some(path) => {
                    validate_drive_path(path, "cache_root")?;
                    Some(path.to_string())
                }
            }
        } else {
            None
        };

        // Events are not collection facts (§3.3): the store may never live
        // inside an archive's immutable object store or its derived tree. The
        // full-text cache is a deletable derivative of live sources, so it must
        // never sit inside an Archive tree either.
        let mut reserved_paths: Vec<(&str, &str)> = Vec::new();
        if let Some(path) = telemetry.as_ref().and_then(|telemetry| telemetry.path.as_deref()) {
            reserved_paths.push(("telemetry.path", path));
        }
        if let Some(path) = cache_root.as_deref() {
            reserved_paths.push(("cache_root", path));
        }
        for (key, path) in reserved_paths {
            for source in &sources {
                if source.kind != SourceKind::Archive {
                    continue;
                }
                for reserved in ["archive", "derived"] {
                    let area = format!(
                        "{}/{}",
                        source.root.trim_end_matches(['/', '\\']),
                        reserved
                    );
                    if is_inside_root(path, &area) {
                        return Err(RecallError::invalid(
                            key,
                            "reserved_area",
                            format!(
                                "{key} must not be inside the archive {reserved} area of source {}",
                                source.source_id
                            ),
                        ));
                    }
                }
            }
        }

        Ok(RecallConfig {
            version,
            sources,
            source_order,
            interpreter,
            domain,
            telemetry,
            cache_root,
            unsupported_sources,
            unsupported_keys,
        })
    }
}

/// `Some` for an object with a non-empty string `source_id` and a string
/// `kind` that [`SourceKind::parse`] does not know. Only `source_id` and `kind`
/// are judged here; the entry's other fields are not validated because this
/// binary cannot know their shape for a future kind. A `kind` that is a
/// spelling variant of a known kind (`claude-code`, ` Claude_Code `) is not
/// treated as a future kind: it is left to [`parse_source`], which reports
/// `unsupported_kind` exactly as before, so typos are never silently skipped.
fn unknown_kind_source(value: &Value) -> Option<UnsupportedSource> {
    let map = value.as_object()?;
    let kind = map.get("kind")?.as_str()?;
    if SourceKind::parse(kind).is_some() {
        return None;
    }
    let folded = kind.trim().to_ascii_lowercase().replace('-', "_");
    if SourceKind::parse(&folded).is_some() {
        return None;
    }
    let source_id = map.get("source_id")?.as_str()?;
    if source_id.trim().is_empty() || kind.trim().is_empty() {
        return None;
    }
    Some(UnsupportedSource {
        source_id: source_id.to_string(),
        kind: kind.to_string(),
        enabled: map.get("enabled").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// `docs` file globs: relative patterns with `/` separators. An absolute path,
/// a drive letter or a `..` segment is a configuration mistake, not a pattern.
fn parse_glob_list(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Vec<String>, RecallError> {
    let Some(value) = map.get(key) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| invalid_type(&format!("{path}.{key}"), "array"))?;
    let mut out = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        let pattern = item.as_str().ok_or_else(|| {
            invalid_type(&format!("{path}.{key}[{index}]"), "string")
        })?;
        let normalized = pattern.replace('\\', "/");
        let bad = normalized.is_empty()
            || normalized.starts_with('/')
            || normalized.contains(':')
            || normalized.split('/').any(|part| part == "..");
        if bad {
            return Err(RecallError::invalid(
                &format!("{path}.{key}[{index}]"),
                "invalid_glob",
                "glob patterns must be relative, without drive letters or '..'",
            ));
        }
        out.push(normalized);
    }
    Ok(out)
}

fn parse_source(value: &Value, index: usize, v2: bool) -> Result<SourceConfig, RecallError> {
    let path = format!("sources[{index}]");
    let map = as_object(value, &path)?;
    // `db`/`storage` are only known keys for session-file/SQLite sources.
    let kind_raw = require(map, "kind", &path)?;
    let kind_str = kind_raw
        .as_str()
        .ok_or_else(|| invalid_type(&format!("{path}.kind"), "string"))?;
    let kind = SourceKind::parse(kind_str).ok_or_else(|| {
        RecallError::invalid(
            &format!("{path}.kind"),
            "unsupported_kind",
            "kind must be one of kb, device, opencode, codex, claude_code, dsh, hermes, archive",
        )
    })?;
    if kind == SourceKind::Archive && !v2 {
        return Err(RecallError::invalid(
            &format!("{path}.kind"),
            "archive_requires_v2",
            "archive sources are only allowed in configuration version 2",
        ));
    }
    let mut allowed = vec!["source_id", "kind", "root", "enabled"];
    if matches!(kind, SourceKind::Opencode | SourceKind::Codex) {
        allowed.push("db");
        allowed.push("storage");
    } else if kind == SourceKind::Hermes {
        allowed.push("db");
    }
    if kind == SourceKind::Docs {
        allowed.push("include");
        allowed.push("exclude");
    }
    reject_unknown(map, &allowed, &path)?;

    let source_id = require(map, "source_id", &path)?;
    let source_id = match source_id {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::String(_) => {
            return Err(RecallError::invalid(
                &format!("{path}.source_id"),
                "empty_source_id",
                "source_id must be a non-empty string",
            ))
        }
        _ => return Err(invalid_type(&format!("{path}.source_id"), "string")),
    };

    let root = require(map, "root", &path)?;
    let root = root
        .as_str()
        .ok_or_else(|| invalid_type(&format!("{path}.root"), "string"))?;
    validate_drive_path(root, &format!("{path}.root"))?;

    let enabled = require(map, "enabled", &path)?;
    let enabled = enabled
        .as_bool()
        .ok_or_else(|| invalid_type(&format!("{path}.enabled"), "boolean"))?;

    let db = optional_path(map, "db", &path)?;
    let storage = optional_path(map, "storage", &path)?;
    for (key, opt) in [("db", &db), ("storage", &storage)] {
        if let Some(child) = opt {
            let child_path = format!("{path}.{key}");
            validate_drive_path(child, &child_path)?;
            if !is_inside_root(child, root) {
                return Err(RecallError::invalid(
                    &child_path,
                    "outside_root",
                    format!("{key} must be located inside the source root"),
                ));
            }
        }
    }

    let include = parse_glob_list(map, "include", &path)?;
    let exclude = parse_glob_list(map, "exclude", &path)?;

    Ok(SourceConfig {
        source_id,
        kind,
        root: root.to_string(),
        enabled,
        db: db.map(|s| s.to_string()),
        storage: storage.map(|s| s.to_string()),
        include,
        exclude,
    })
}

fn parse_interpreter(value: &Value, v2: bool) -> Result<InterpreterConfig, RecallError> {
    let map = as_object(value, "interpreter")?;
    reject_unknown(
        map,
        if v2 { &["kb", "embedding"] } else { &["kb"] },
        "interpreter",
    )?;
    let kb = parse_kb(require(map, "kb", "interpreter")?)?;
    let embedding = if v2 {
        match map.get("embedding") {
            None => None,
            Some(value) => Some(EmbeddingModelSpec::parse(value, "interpreter.embedding")?),
        }
    } else {
        None
    };
    Ok(InterpreterConfig { kb, embedding })
}

fn parse_kb(value: &Value) -> Result<KbInterpreter, RecallError> {
    let kb_map = as_object(value, "interpreter.kb")?;
    reject_unknown(kb_map, &["command", "args", "env"], "interpreter.kb")?;

    let command = require(kb_map, "command", "interpreter.kb")?;
    let command = match command {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::String(_) => {
            return Err(RecallError::invalid(
                "interpreter.kb.command",
                "empty_command",
                "command must be a non-empty string",
            ))
        }
        _ => return Err(invalid_type("interpreter.kb.command", "string")),
    };

    let args = match kb_map.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let mut args = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let arg = item.as_str().ok_or_else(|| {
                    invalid_type(&format!("interpreter.kb.args[{index}]"), "string")
                })?;
                args.push(arg.to_string());
            }
            args
        }
        Some(_) => return Err(invalid_type("interpreter.kb.args", "array")),
    };

    let env = match kb_map.get("env") {
        None => BTreeMap::new(),
        Some(Value::Object(entries)) => {
            let mut env = BTreeMap::new();
            for (key, value) in entries {
                let value = value.as_str().ok_or_else(|| {
                    invalid_type(&format!("interpreter.kb.env.{key}"), "string")
                })?;
                env.insert(key.clone(), value.to_string());
            }
            env
        }
        Some(_) => return Err(invalid_type("interpreter.kb.env", "object")),
    };

    Ok(KbInterpreter {
        command,
        args,
        env,
    })
}

fn parse_telemetry(value: &Value) -> Result<TelemetryConfig, RecallError> {
    let map = as_object(value, "telemetry")?;
    reject_unknown(
        map,
        &["enabled", "path", "query_text_ttl_days", "retention_days"],
        "telemetry",
    )?;
    let enabled = require(map, "enabled", "telemetry")?
        .as_bool()
        .ok_or_else(|| invalid_type("telemetry.enabled", "boolean"))?;
    let path = optional_path(map, "path", "telemetry")?;
    if let Some(path) = path {
        validate_drive_path(path, "telemetry.path")?;
    }
    let query_text_ttl_days = match map.get("query_text_ttl_days") {
        None => 7,
        Some(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| invalid_type("telemetry.query_text_ttl_days", "integer"))?;
            if !(1..=90).contains(&number) {
                return Err(RecallError::invalid(
                    "telemetry.query_text_ttl_days",
                    "out_of_range",
                    "telemetry.query_text_ttl_days must be an integer in 1..=90",
                ));
            }
            number as u32
        }
    };
    let retention_days = match map.get("retention_days") {
        None => crate::recall::telemetry::DEFAULT_RETENTION_DAYS,
        Some(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| invalid_type("telemetry.retention_days", "integer"))?;
            if !(crate::recall::telemetry::MIN_RETENTION_DAYS as u64
                ..=crate::recall::telemetry::MAX_RETENTION_DAYS as u64)
                .contains(&number)
            {
                return Err(RecallError::invalid(
                    "telemetry.retention_days",
                    "out_of_range",
                    format!(
                        "telemetry.retention_days must be an integer in {}..={}",
                        crate::recall::telemetry::MIN_RETENTION_DAYS,
                        crate::recall::telemetry::MAX_RETENTION_DAYS
                    ),
                ));
            }
            number as u32
        }
    };
    if enabled && path.is_none() {
        return Err(RecallError::invalid(
            "telemetry.path",
            "required_when_enabled",
            "telemetry.path is required when telemetry.enabled is true",
        ));
    }
    Ok(TelemetryConfig {
        enabled,
        path: path.map(|value| value.to_string()),
        query_text_ttl_days,
        retention_days,
    })
}

fn as_object<'a>(
    value: &'a Value,
    path: &str,
) -> Result<&'a Map<String, Value>, RecallError> {
    value
        .as_object()
        .ok_or_else(|| invalid_type(path, "object"))
}

fn require<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    parent: &str,
) -> Result<&'a Value, RecallError> {
    map.get(key).ok_or_else(|| {
        let path = if parent.is_empty() {
            key.to_string()
        } else {
            format!("{parent}.{key}")
        };
        RecallError::invalid(&path, "missing_key", format!("{path} is required"))
    })
}

fn optional_path<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    parent: &str,
) -> Result<Option<&'a str>, RecallError> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(invalid_type(&format!("{parent}.{key}"), "string")),
    }
}

/// Fold one configuration key for spelling-variant detection: ASCII
/// lower-case and `-` -> `_`. Nothing else folds, so a genuinely new key is
/// still ignored+disclosed while `cache-root` / `Source_Order` stay typos.
fn fold_config_key(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

fn reject_unknown(
    map: &Map<String, Value>,
    allowed: &[&str],
    parent: &str,
) -> Result<(), RecallError> {
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        let path = if parent.is_empty() {
            key.clone()
        } else {
            format!("{parent}.{key}")
        };
        return Err(RecallError::invalid(
            &path,
            "unknown_key",
            format!("unknown configuration key: {path}"),
        ));
    }
    Ok(())
}

fn invalid_type(path: &str, expected: &str) -> RecallError {
    RecallError::invalid(
        path,
        "invalid_type",
        format!("{path} must be {expected}"),
    )
}

fn is_drive_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn is_unc(value: &str) -> bool {
    value.starts_with("//") || value.starts_with(r"\\")
}

pub(crate) fn validate_drive_path(value: &str, path: &str) -> Result<(), RecallError> {
    if is_unc(value) {
        return Err(RecallError::invalid(
            path,
            "unc_path",
            "UNC and extended-length paths are not accepted",
        ));
    }
    if !is_drive_absolute(value) {
        return Err(RecallError::invalid(
            path,
            "not_absolute",
            "path must be a drive-absolute path such as C:/data",
        ));
    }
    if value
        .split(['/', '\\'])
        .any(|segment| segment == ".." || segment == ".")
    {
        return Err(RecallError::invalid(
            path,
            "relative_segment",
            "path must not contain '.' or '..' segments",
        ));
    }
    Ok(())
}

/// Lexically normalized, case-folded path used only for containment checks.
pub fn normalize_for_compare(value: &str) -> String {
    let lowered = value.to_ascii_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_slash = false;
    for ch in lowered.chars() {
        if ch == '/' || ch == '\\' {
            if !last_slash {
                out.push('/');
            }
            last_slash = true;
        } else {
            out.push(ch);
            last_slash = false;
        }
    }
    if out.len() > 3 && out.ends_with('/') {
        out.pop();
    }
    out
}

fn is_inside_root(child: &str, root: &str) -> bool {
    let child = normalize_for_compare(child);
    let root = normalize_for_compare(root);
    let prefix = if root.ends_with('/') {
        root
    } else {
        format!("{root}/")
    };
    child.len() > prefix.len() && child.starts_with(&prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::contract::codes;

    fn v1_config() -> Value {
        serde_json::json!({
            "version": 1,
            "sources": [
                {"source_id": "kb", "kind": "kb", "root": "C:/vault", "enabled": true}
            ],
            "source_order": ["kb"],
            "interpreter": {"kb": {"command": "C:/python.exe"}}
        })
    }

    fn v2_config() -> Value {
        serde_json::json!({
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

    #[test]
    fn accepts_minimal_v1_and_normalizes_in_memory() {
        let cfg = RecallConfig::from_value(&v1_config()).expect("valid");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.domain, DEFAULT_DOMAIN);
        assert!(cfg.telemetry.is_none());
        assert!(cfg.interpreter.embedding.is_none());
        assert_eq!(cfg.source_order, vec!["kb".to_string()]);
        assert_eq!(cfg.interpreter.kb.command, "C:/python.exe");
        assert!(cfg.interpreter.kb.args.is_empty());
        assert!(cfg.interpreter.kb.env.is_empty());
    }

    #[test]
    fn accepts_minimal_v2() {
        let cfg = RecallConfig::from_value(&v2_config()).expect("valid");
        assert_eq!(cfg.version, 2);
        assert_eq!(cfg.domain, "default");
        assert!(!cfg.telemetry_enabled());
    }

    #[test]
    fn v1_rejects_archive_domain_and_telemetry() {
        let mut value = v1_config();
        value["sources"][0]["kind"] = serde_json::json!("archive");
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["reason"], "archive_requires_v2");

        let mut value = v1_config();
        value["domain"] = serde_json::json!("work");
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["path"], "domain");

        let mut value = v1_config();
        value["telemetry"] = serde_json::json!({"enabled": false});
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["path"], "telemetry");
    }

    #[test]
    fn v2_requires_domain_and_rejects_unknown_keys() {
        let mut value = v2_config();
        value.as_object_mut().unwrap().remove("domain");
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["path"], "domain");

        // A genuinely unknown top-level key is ignored and disclosed (an
        // older process keeps serving a newer configuration).
        let mut value = v2_config();
        value["extra"] = serde_json::json!(1);
        let cfg = RecallConfig::from_value(&value).expect("unknown top-level key is ignored");
        assert_eq!(cfg.unsupported_keys, vec!["extra".to_string()]);
        let warnings = cfg.unsupported_key_warnings("search");
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].code,
            crate::recall::contract::WARN_CONFIG_KEY_UNSUPPORTED
        );
        assert_eq!(
            warnings[0].detail.as_ref().unwrap()["keys"],
            serde_json::json!(["extra"])
        );

        // A spelling variant of a known key is still a typo, not a new key.
        for variant in ["cache-root", "Source_Order", "DOMAIN"] {
            let mut value = v2_config();
            value[variant] = serde_json::json!(1);
            let err = RecallConfig::from_value(&value).unwrap_err();
            assert_eq!(err.detail.unwrap()["path"], variant);
        }

        for domain in ["", "Work", "-work", "work space", &"a".repeat(65)] {
            let mut value = v2_config();
            value["domain"] = serde_json::json!(domain);
            assert!(
                RecallConfig::from_value(&value).is_err(),
                "domain {domain:?} must be rejected"
            );
        }
    }

    #[test]
    fn v2_telemetry_validates_enabled_path_and_ttl() {
        let mut value = v2_config();
        value["telemetry"] = serde_json::json!({"enabled": true});
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["path"], "telemetry.path");

        let mut value = v2_config();
        value["telemetry"] = serde_json::json!({
            "enabled": true,
            "path": "C:/state/retrieval_events.sqlite",
            "query_text_ttl_days": 0
        });
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(
            err.detail.unwrap()["path"],
            "telemetry.query_text_ttl_days"
        );

        let mut value = v2_config();
        value["telemetry"] = serde_json::json!({
            "enabled": true,
            "path": "C:/state/retrieval_events.sqlite"
        });
        let cfg = RecallConfig::from_value(&value).expect("enabled telemetry");
        assert!(cfg.telemetry_enabled());
        assert_eq!(cfg.telemetry.unwrap().query_text_ttl_days, 7);
    }

    #[test]
    fn v2_accepts_archive_and_embedding_spec() {
        let mut value = v2_config();
        value["sources"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "source_id": "ar", "kind": "archive",
                "root": "C:/archive", "enabled": false
            }));
        value["interpreter"]["embedding"] = serde_json::json!({
            "model_id": "bge-small-zh-v1.5",
            "dimension": 512,
            "normalization": "l2",
            "backend": "python"
        });
        let cfg = RecallConfig::from_value(&value).expect("archive + embedding");
        assert_eq!(cfg.sources[1].kind, SourceKind::Archive);
        assert_eq!(
            cfg.interpreter
                .embedding
                .as_ref()
                .unwrap()
                .generation_key(),
            "embed-v1;model=bge-small-zh-v1.5;dim=512;norm=l2;backend=python"
        );
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let mut value = v1_config();
        value["extra"] = serde_json::json!(1);
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.code, codes::INVALID_REQUEST);
        assert_eq!(err.detail.unwrap()["path"], "extra");
    }

    #[test]
    fn rejects_missing_interpreter_command() {
        let mut value = v1_config();
        value["interpreter"] = serde_json::json!({"kb": {}});
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["reason"], "missing_key");
    }

    #[test]
    fn rejects_unc_and_relative_roots() {
        for root in [r"\\server\share", "//server/share", "relative/path", r"\\?\C:\x"] {
            let mut value = v1_config();
            value["sources"][0]["root"] = serde_json::json!(root);
            assert!(
                RecallConfig::from_value(&value).is_err(),
                "root {root} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_db_outside_root_case_insensitively() {
        let mut value = serde_json::json!({
            "version": 1,
            "sources": [{
                "source_id": "oc", "kind": "opencode", "root": "C:/Root",
                "db": "c:/root/db.sqlite", "enabled": true
            }],
            "source_order": ["oc"],
            "interpreter": {"kb": {"command": "C:/python.exe"}}
        });
        RecallConfig::from_value(&value).expect("case-insensitive containment");

        value["sources"][0]["db"] = serde_json::json!("C:/Other/db.sqlite");
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["path"], "sources[0].db");
    }

    #[test]
    fn rejects_zero_enabled_sources_and_empty_sources() {
        let mut value = v1_config();
        value["sources"][0]["enabled"] = serde_json::json!(false);
        value["source_order"] = serde_json::json!([]);
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["reason"], "no_enabled_source");

        let mut value = v1_config();
        value["sources"] = serde_json::json!([]);
        value["source_order"] = serde_json::json!([]);
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["reason"], "empty_sources");
    }

    #[test]
    fn rejects_source_order_mismatch() {
        let mut value = v1_config();
        value["source_order"] = serde_json::json!(["kb", "device"]);
        let err = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(err.detail.unwrap()["reason"], "source_order_mismatch");
    }

    #[test]
    fn root_normalization_folds_case_and_separators() {
        assert_eq!(normalize_for_compare(r"C:\WT\x\"), "c:/wt/x");
        assert_eq!(normalize_for_compare("c:/wt/x"), "c:/wt/x");
        assert_eq!(normalize_for_compare("C:/"), "c:/");
    }
}
