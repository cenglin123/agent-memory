//! Embedding execution for Archive hybrid recall (P3).
//!
//! A Rust process never loads a model: the configured Python helper
//! (`assets/scripts/recall_embedding.py --embed`) is the only place a model
//! exists. The plan below carries the frozen model identity plus the interpreter
//! and helper location, and turns a batch of texts into one vector per text.
//!
//! The test-only `fake` backend is implemented locally (deterministic character
//! n-gram hashing). It has no model, no network and no subprocess, so the hybrid
//! pipeline can be exercised offline; it is only reachable when the configuration
//! explicitly says `backend=fake`.
//!
//! Every failure is a degradation: it becomes an explicit reason string and the
//! lexical path continues.

use crate::recall::config::RecallConfig;
use crate::recall::semantic::model::{EmbeddingBackend, EmbeddingModelSpec, Normalization};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Helper script, resolved relative to the running binary (never from a home
/// directory or a default data root).
pub const EMBEDDING_SCRIPT: &str = "recall_embedding.py";
pub const HELPER_SCHEMA_VERSION: u64 = 1;
/// Upper bound on one batch handed to the helper.
pub const MAX_BATCH: usize = 64;

#[derive(Debug, Clone)]
pub struct EmbeddingPlan {
    pub spec: EmbeddingModelSpec,
    /// Interpreter that runs the helper (the explicit KB interpreter).
    pub interpreter: String,
    pub interpreter_args: Vec<String>,
    pub interpreter_env: BTreeMap<String, String>,
    /// Resolved helper script; `None` is an explicit degradation, never a guess.
    pub script: Option<PathBuf>,
    /// Loopback-only Ollama endpoint; never inferred from a non-loopback host.
    pub ollama_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedError {
    /// Machine-readable degradation reason.
    pub reason: String,
    pub detail: Option<String>,
}

impl EmbedError {
    fn new(reason: &str) -> Self {
        EmbedError {
            reason: reason.to_string(),
            detail: None,
        }
    }

    fn with(reason: &str, detail: String) -> Self {
        EmbedError {
            reason: reason.to_string(),
            detail: Some(detail),
        }
    }
}

impl EmbeddingPlan {
    /// Build a plan from the v2 configuration. `None` means the semantic path is
    /// not configured (lexical only).
    pub fn from_config(config: &RecallConfig) -> Option<Self> {
        let mut spec = config.interpreter.embedding.as_ref()?.clone();
        let ollama_url = spec
            .endpoint
            .take()
            .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
        Some(EmbeddingPlan {
            spec,
            interpreter: config.interpreter.kb.command.clone(),
            interpreter_args: config.interpreter.kb.args.clone(),
            interpreter_env: config.interpreter.kb.env.clone(),
            script: resolve_script(),
            ollama_url,
        })
    }

    /// Explicit plan for tests and non-CLI callers.
    pub fn for_spec(spec: EmbeddingModelSpec) -> Self {
        EmbeddingPlan {
            spec,
            interpreter: "python".to_string(),
            interpreter_args: Vec::new(),
            interpreter_env: BTreeMap::new(),
            script: resolve_script(),
            ollama_url: "http://127.0.0.1:11434".to_string(),
        }
    }

    pub fn generation_key(&self) -> String {
        self.spec.generation_key()
    }

    pub fn backend(&self) -> &'static str {
        self.spec.backend.as_str()
    }

    /// `true` when the helper can be invoked (or when the fake backend runs
    /// locally). A missing script for the fake backend is harmless.
    pub fn helper_ready(&self) -> bool {
        self.spec.backend == EmbeddingBackend::Fake || self.script.is_some()
    }

    /// One batch of vectors. Every returned vector has exactly
    /// `spec.dimension` floats.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if self.spec.backend == EmbeddingBackend::Fake {
            return Ok(texts
                .iter()
                .map(|text| fake_vector(text, self.spec.dimension as usize, self.spec.normalization))
                .collect());
        }
        let Some(script) = self.script.as_ref() else {
            return Err(EmbedError::new("script_unavailable"));
        };
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for batch in texts.chunks(MAX_BATCH) {
            out.extend(self.embed_remote(script, batch)?);
        }
        Ok(out)
    }

    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vectors = self.embed_batch(&[text.to_string()])?;
        vectors
            .pop()
            .ok_or_else(|| EmbedError::new("empty_embedding_response"))
    }

    fn embed_remote(&self, script: &Path, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let request = json!({
            "schema_version": HELPER_SCHEMA_VERSION,
            "texts": texts,
            "ollama_url": self.ollama_url,
            "model": {
                "model_id": self.spec.model_id,
                "dimension": self.spec.dimension,
                "normalization": self.spec.normalization.as_str(),
                "backend": self.spec.backend.as_str(),
                "model_path": self.spec.model_path,
            }
        });
        let mut command = Command::new(&self.interpreter);
        command.args(&self.interpreter_args);
        command.arg(script);
        command.arg("--embed");
        command.env("HF_HUB_OFFLINE", "1");
        command.env("TRANSFORMERS_OFFLINE", "1");
        for (key, value) in &self.interpreter_env {
            command.env(key, value);
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| EmbedError::with("python_unavailable", error.to_string()))?;
        {
            use std::io::Write;
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| EmbedError::new("python_unavailable"))?;
            stdin
                .write_all(request.to_string().as_bytes())
                .map_err(|error| EmbedError::with("embedding_failed", error.to_string()))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| EmbedError::with("embedding_failed", error.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
            EmbedError::with(
                "embedding_failed",
                format!("helper output is not JSON: {error}"),
            )
        })?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("embedding_failed");
            let detail = value
                .get("detail")
                .map(|detail| detail.to_string())
                .or_else(|| value.get("error").map(|error| error.to_string()));
            return Err(EmbedError {
                reason: reason.to_string(),
                detail,
            });
        }
        let backend = value
            .get("backend")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if backend != self.spec.backend.as_str() {
            return Err(EmbedError::with(
                "backend_mismatch",
                format!(
                    "helper reported backend {backend:?}, configuration says {:?}",
                    self.spec.backend.as_str()
                ),
            ));
        }
        let vectors = value
            .get("vectors")
            .and_then(Value::as_array)
            .ok_or_else(|| EmbedError::new("empty_embedding_response"))?;
        if vectors.len() != texts.len() {
            return Err(EmbedError::with(
                "embedding_failed",
                format!(
                    "helper returned {} vectors for {} texts",
                    vectors.len(),
                    texts.len()
                ),
            ));
        }
        let dimension = self.spec.dimension as usize;
        let mut out = Vec::with_capacity(vectors.len());
        for vector in vectors {
            let values: Option<Vec<f32>> = vector
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_f64().map(|value| value as f32))
                        .collect()
                });
            let values = values.ok_or_else(|| EmbedError::new("dimension_mismatch"))?;
            if values.len() != dimension {
                return Err(EmbedError::with(
                    "dimension_mismatch",
                    format!(
                        "helper returned dimension {}, configuration says {dimension}",
                        values.len()
                    ),
                ));
            }
            out.push(values);
        }
        Ok(out)
    }
}

/// Outcome of embedding one query.
#[derive(Debug, Clone)]
pub enum SemanticState {
    /// No embedding model is configured in this environment.
    NotConfigured,
    /// The model exists but produced no vector; every source degrades with this
    /// same reason.
    Unavailable(String),
    /// The query vector and its norm.
    Ready { vector: Vec<f32>, norm: f32 },
}

/// The query vector of one search request, embedded at most once no matter how
/// many sources ask for it.
///
/// A source's semantic branch depends on the query vector alone, never on the
/// source, so every indexed source shares one embedding. Without this each of
/// them would spawn its own helper process for the identical query.
pub struct SemanticQuery {
    plan: Option<EmbeddingPlan>,
    query: String,
    state: OnceLock<SemanticState>,
}

impl SemanticQuery {
    pub fn new(plan: Option<EmbeddingPlan>, query: &str) -> Self {
        SemanticQuery {
            plan,
            query: query.to_string(),
            state: OnceLock::new(),
        }
    }

    /// The configured model identity, when there is one. Callers use it to
    /// validate a source's vector cache; it never triggers an embedding.
    pub fn plan(&self) -> Option<&EmbeddingPlan> {
        self.plan.as_ref()
    }

    /// Embed on first use; every later caller sees the same outcome.
    pub fn state(&self) -> &SemanticState {
        self.state.get_or_init(|| self.embed())
    }

    fn embed(&self) -> SemanticState {
        let Some(plan) = self.plan.as_ref() else {
            return SemanticState::NotConfigured;
        };
        if !plan.helper_ready() {
            return SemanticState::Unavailable("embedding_helper_unavailable".to_string());
        }
        match plan.embed_one(&self.query) {
            Ok(vector) => {
                let norm = crate::recall::semantic::index::vector_norm(&vector);
                SemanticState::Ready { vector, norm }
            }
            Err(error) => SemanticState::Unavailable(match error.detail {
                Some(detail) => format!("{}: {}", error.reason, detail),
                None => error.reason,
            }),
        }
    }
}

/// Locate the helper script beside the running binary or up its ancestors.
///
/// Candidates (first hit wins): `<ancestor>/scripts/recall_embedding.py` and
/// `<ancestor>/assets/scripts/recall_embedding.py` for the executable directory
/// and its ancestors. Distribution layout (`assets/dist/agent-memory/agent-memory.exe`)
/// and the test layout (`agent-memory/target/debug/`) both resolve.
pub fn resolve_script() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    for root in roots {
        let mut cursor = Some(root.as_path());
        let mut depth = 0;
        while let Some(directory) = cursor {
            for candidate in [
                directory.join("scripts").join(EMBEDDING_SCRIPT),
                directory.join("assets").join("scripts").join(EMBEDDING_SCRIPT),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            depth += 1;
            if depth > 6 {
                break;
            }
            cursor = directory.parent();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Deterministic fake backend (test-only)
// ---------------------------------------------------------------------------

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

fn fnv1a64(data: &[u8]) -> u64 {
    let mut value = FNV_OFFSET;
    for byte in data {
        value ^= *byte as u64;
        value = value.wrapping_mul(FNV_PRIME);
    }
    value
}

/// Mirror of `fake_vector` in `assets/scripts/recall_embedding.py`.
pub fn fake_vector(text: &str, dimension: usize, normalization: Normalization) -> Vec<f32> {
    let data = text.as_bytes();
    let mut vector = vec![0.0f32; dimension];
    if data.is_empty() || dimension == 0 {
        return vector;
    }
    if data.len() <= 3 {
        add_gram(&mut vector, data);
    } else {
        for window in data.windows(3) {
            add_gram(&mut vector, window);
        }
    }
    if normalization == Normalization::L2 {
        let norm: f32 = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut vector {
                *value /= norm;
            }
        }
    }
    vector
}

fn add_gram(vector: &mut [f32], gram: &[u8]) {
    let hashed = fnv1a64(gram);
    let index = (hashed % vector.len() as u64) as usize;
    let sign = if (hashed >> 63) & 1 == 1 { -1.0 } else { 1.0 };
    vector[index] += sign;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::semantic::model::Normalization;

    fn fake_spec() -> EmbeddingModelSpec {
        EmbeddingModelSpec {
            model_id: "fake-hash-v1".to_string(),
            dimension: 32,
            normalization: Normalization::L2,
            backend: EmbeddingBackend::Fake,
            model_path: None,
            endpoint: None,
        }
    }

    #[test]
    fn fake_vectors_are_deterministic_and_normalized() {
        let plan = EmbeddingPlan::for_spec(fake_spec());
        let first = plan.embed_one("缓存失效策略").unwrap();
        let second = plan.embed_one("缓存失效策略").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        let norm: f32 = first.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "{norm}");
        let other = plan.embed_one("unrelated text").unwrap();
        assert_ne!(first, other);
    }

    #[test]
    fn fake_backend_does_not_need_a_script_or_a_model() {
        let mut plan = EmbeddingPlan::for_spec(fake_spec());
        plan.script = None;
        assert!(plan.helper_ready());
        assert!(plan.embed_one("x").is_ok());
    }

    #[test]
    fn missing_script_is_an_explicit_degradation() {
        let mut spec = fake_spec();
        spec.backend = EmbeddingBackend::Python;
        let mut plan = EmbeddingPlan::for_spec(spec);
        plan.script = None;
        let error = plan.embed_one("x").unwrap_err();
        assert_eq!(error.reason, "script_unavailable");
    }

    #[test]
    fn generation_key_round_trips_the_spec() {
        let plan = EmbeddingPlan::for_spec(fake_spec());
        assert_eq!(
            plan.generation_key(),
            "embed-v1;model=fake-hash-v1;dim=32;norm=l2;backend=fake"
        );
    }

    #[test]
    fn helper_script_is_found_from_the_test_binary() {
        // The workspace keeps the helper at `<suite>/assets/scripts/`.
        assert!(resolve_script().is_some());
    }
}
