//! Embedding model identity used by the retrieval generation key.
//!
//! The key includes the model id, dimension and normalization, so switching a
//! model can never silently reuse an old vector generation. P0 only validates
//! and renders the identity; P3 owns the actual embedding subprocess.

use crate::recall::config::validate_drive_path;
use crate::recall::contract::RecallError;
use serde_json::{Map, Value};

pub const MAX_MODEL_ID_LEN: usize = 128;
pub const MAX_DIMENSION: u64 = 8_192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Normalization {
    L2,
    None,
}

impl Normalization {
    pub fn as_str(&self) -> &'static str {
        match self {
            Normalization::L2 => "l2",
            Normalization::None => "none",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "l2" => Some(Normalization::L2),
            "none" => Some(Normalization::None),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingBackend {
    Python,
    Ollama,
    /// Deterministic, model-free hashing used only when the configuration says
    /// so explicitly (tests and offline evaluation). Never a fallback.
    Fake,
}

impl EmbeddingBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbeddingBackend::Python => "python",
            EmbeddingBackend::Ollama => "ollama",
            EmbeddingBackend::Fake => "fake",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "python" => Some(EmbeddingBackend::Python),
            "ollama" => Some(EmbeddingBackend::Ollama),
            "fake" => Some(EmbeddingBackend::Fake),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingModelSpec {
    pub model_id: String,
    pub dimension: u64,
    pub normalization: Normalization,
    pub backend: EmbeddingBackend,
    /// Explicit offline cache/model location; never discovered from a home
    /// directory or an environment variable.
    pub model_path: Option<String>,
    /// Loopback endpoint of an Ollama-compatible embedding server.  A local
    /// sidecar (small ONNX model, no Ollama dependency) can serve it; the
    /// default stays Ollama's port when unset.
    pub endpoint: Option<String>,
}

impl EmbeddingModelSpec {
    pub fn parse(value: &Value, path: &str) -> Result<Self, RecallError> {
        let map = value
            .as_object()
            .ok_or_else(|| invalid_type(path, "object"))?;
        reject_unknown(
            map,
            &[
                "model_id",
                "dimension",
                "normalization",
                "backend",
                "model_path",
                "endpoint",
            ],
            path,
        )?;

        let model_id = required_string(map, "model_id", path)?;
        if model_id.len() > MAX_MODEL_ID_LEN
            || !model_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-'))
        {
            return Err(RecallError::invalid(
                &format!("{path}.model_id"),
                "invalid_model_id",
                format!(
                    "model_id must be 1..={MAX_MODEL_ID_LEN} ASCII letters, digits or . _ : / -"
                ),
            ));
        }

        let dimension = match map.get("dimension") {
            Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
                invalid_type(&format!("{path}.dimension"), "positive integer")
            })?,
            Some(_) => {
                return Err(invalid_type(
                    &format!("{path}.dimension"),
                    "positive integer",
                ))
            }
            None => return Err(missing(&format!("{path}.dimension"))),
        };
        if dimension == 0 || dimension > MAX_DIMENSION {
            return Err(RecallError::invalid(
                &format!("{path}.dimension"),
                "out_of_range",
                format!("dimension must be an integer in 1..={MAX_DIMENSION}"),
            ));
        }

        let normalization = required_string(map, "normalization", path)?;
        let normalization = Normalization::parse(&normalization).ok_or_else(|| {
            RecallError::invalid(
                &format!("{path}.normalization"),
                "unsupported_normalization",
                "normalization must be l2 or none",
            )
        })?;

        let backend = required_string(map, "backend", path)?;
        let backend = EmbeddingBackend::parse(&backend).ok_or_else(|| {
            RecallError::invalid(
                &format!("{path}.backend"),
                "unsupported_backend",
                "backend must be python, ollama or fake",
            )
        })?;

        let model_path = match map.get("model_path") {
            None => None,
            Some(Value::String(value)) if !value.is_empty() => {
                validate_drive_path(value, &format!("{path}.model_path"))?;
                Some(value.clone())
            }
            Some(Value::String(_)) => {
                return Err(RecallError::invalid(
                    &format!("{path}.model_path"),
                    "empty_value",
                    "model_path must not be empty",
                ))
            }
            Some(_) => return Err(invalid_type(&format!("{path}.model_path"), "string")),
        };

        let endpoint = match map.get("endpoint") {
            None => None,
            Some(Value::String(value)) if !value.is_empty() => {
                let trimmed = value.trim_end_matches('/');
                let host_ok = trimmed.starts_with("http://127.0.0.1:")
                    || trimmed.starts_with("http://localhost:")
                    || trimmed.starts_with("http://[::1]:");
                if !host_ok {
                    return Err(RecallError::invalid(
                        &format!("{path}.endpoint"),
                        "not_loopback",
                        "endpoint must be a loopback http URL (127.0.0.1, localhost or [::1])",
                    ));
                }
                Some(trimmed.to_string())
            }
            Some(Value::String(_)) => {
                return Err(RecallError::invalid(
                    &format!("{path}.endpoint"),
                    "empty_value",
                    "endpoint must not be empty",
                ))
            }
            Some(_) => return Err(invalid_type(&format!("{path}.endpoint"), "string")),
        };

        Ok(EmbeddingModelSpec {
            model_id,
            dimension,
            normalization,
            backend,
            model_path,
            endpoint,
        })
    }

    /// Deterministic generation identity. Changing any component forces a new
    /// full rebuild instead of mixing vectors from two models.
    pub fn generation_key(&self) -> String {
        format!(
            "embed-v1;model={};dim={};norm={};backend={}",
            self.model_id,
            self.dimension,
            self.normalization.as_str(),
            self.backend.as_str()
        )
    }
}

fn reject_unknown(
    map: &Map<String, Value>,
    allowed: &[&str],
    parent: &str,
) -> Result<(), RecallError> {
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        let path = format!("{parent}.{key}");
        return Err(RecallError::invalid(
            &path,
            "unknown_key",
            format!("unknown key: {path}"),
        ));
    }
    Ok(())
}

fn required_string(
    map: &Map<String, Value>,
    key: &str,
    parent: &str,
) -> Result<String, RecallError> {
    match map.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(Value::String(_)) => Err(RecallError::invalid(
            &format!("{parent}.{key}"),
            "empty_value",
            "value must not be empty",
        )),
        Some(_) => Err(invalid_type(&format!("{parent}.{key}"), "string")),
        None => Err(missing(&format!("{parent}.{key}"))),
    }
}

fn missing(path: &str) -> RecallError {
    RecallError::invalid(path, "missing_key", format!("{path} is required"))
}

fn invalid_type(path: &str, expected: &str) -> RecallError {
    RecallError::invalid(path, "invalid_type", format!("{path} must be {expected}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> EmbeddingModelSpec {
        EmbeddingModelSpec {
            model_id: "bge-small-zh-v1.5".into(),
            dimension: 512,
            normalization: Normalization::L2,
            backend: EmbeddingBackend::Python,
            model_path: Some("C:/models/bge".into()),
            endpoint: None,
        }
    }

    #[test]
    fn endpoint_is_accepted_only_on_loopback() {
        let mut value = json!({
            "model_id": "bge-small-zh-v1.5",
            "dimension": 512,
            "normalization": "l2",
            "backend": "ollama",
            "endpoint": "http://127.0.0.1:11435/",
        });
        let parsed = EmbeddingModelSpec::parse(&value, "interpreter.embedding").expect("loopback");
        assert_eq!(parsed.endpoint.as_deref(), Some("http://127.0.0.1:11435"));
        value["endpoint"] = json!("http://10.0.0.5:11435");
        let error = EmbeddingModelSpec::parse(&value, "interpreter.embedding").expect_err("remote");
        let detail = error.detail.expect("detail");
        assert_eq!(detail["reason"], "not_loopback");
    }

    #[test]
    fn generation_key_contains_model_dimension_and_normalization() {
        let key = spec().generation_key();
        assert_eq!(
            key,
            "embed-v1;model=bge-small-zh-v1.5;dim=512;norm=l2;backend=python"
        );
        let mut changed = spec();
        changed.normalization = Normalization::None;
        assert_ne!(key, changed.generation_key());
        let mut changed = spec();
        changed.dimension = 1024;
        assert_ne!(key, changed.generation_key());
        let mut changed = spec();
        changed.model_id = "other".into();
        assert_ne!(key, changed.generation_key());
    }

    #[test]
    fn parses_a_complete_spec_and_rejects_bad_shapes() {
        let parsed = EmbeddingModelSpec::parse(
            &json!({
                "model_id": "bge-small-zh-v1.5",
                "dimension": 512,
                "normalization": "l2",
                "backend": "python",
                "model_path": "C:/models/bge"
            }),
            "interpreter.embedding",
        )
        .expect("valid spec");
        assert_eq!(parsed.generation_key(), spec().generation_key());

        type Mutate = fn(&mut Value);
        let cases: [(Mutate, &str); 7] = [
            (
                |value: &mut Value| value["extra"] = json!(1),
                "interpreter.embedding.extra",
            ),
            (
                |value: &mut Value| {
                    value.as_object_mut().unwrap().remove("dimension");
                },
                "interpreter.embedding.dimension",
            ),
            (
                |value: &mut Value| value["dimension"] = json!(0),
                "interpreter.embedding.dimension",
            ),
            (
                |value: &mut Value| value["dimension"] = json!(8193),
                "interpreter.embedding.dimension",
            ),
            (
                |value: &mut Value| value["normalization"] = json!("l1"),
                "interpreter.embedding.normalization",
            ),
            (
                |value: &mut Value| value["backend"] = json!("onnx"),
                "interpreter.embedding.backend",
            ),
            (
                |value: &mut Value| value["model_path"] = json!("relative/x"),
                "interpreter.embedding.model_path",
            ),
        ];
        for (mutate, path) in cases {
            let mut value = json!({
                "model_id": "bge-small-zh-v1.5",
                "dimension": 512,
                "normalization": "l2",
                "backend": "python",
                "model_path": "C:/models/bge"
            });
            mutate(&mut value);
            let error = EmbeddingModelSpec::parse(&value, "interpreter.embedding")
                .expect_err(path);
            assert_eq!(
                error.detail.as_ref().unwrap()["path"].as_str().unwrap(),
                path
            );
        }
    }
}
