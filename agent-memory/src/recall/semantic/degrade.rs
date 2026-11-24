//! Executable degradation matrix for the optional semantic path.
//!
//! P0 has no embedding runner. The matrix is still executable: callers pass the
//! probed facts (Python, model cache, backend availability) and receive an
//! explicit status. Lexical recall is never conditional on this result.

use crate::recall::semantic::model::{EmbeddingBackend, EmbeddingModelSpec};

/// Facts a host probe may establish without loading a model. P0 tests feed
/// these synthetically; `assets/scripts/recall_embedding.py --probe` produces
/// the same facts offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeFacts {
    pub python_available: bool,
    pub model_available: bool,
    pub backend_available: bool,
}

impl ProbeFacts {
    pub const fn all_available() -> Self {
        ProbeFacts {
            python_available: true,
            model_available: true,
            backend_available: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticStatus {
    /// No embedding spec in the configuration.
    Disabled(&'static str),
    /// A configured capability is missing; lexical recall continues.
    Degraded(&'static str),
    Available,
}

impl SemanticStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SemanticStatus::Disabled(_) => "disabled",
            SemanticStatus::Degraded(_) => "degraded",
            SemanticStatus::Available => "available",
        }
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            SemanticStatus::Disabled(reason) | SemanticStatus::Degraded(reason) => Some(reason),
            SemanticStatus::Available => None,
        }
    }

    pub fn semantic_available(&self) -> bool {
        matches!(self, SemanticStatus::Available)
    }
}

/// Lexical recall is unconditional: no Python, model or Ollama dependency is
/// allowed to disable the word-level path.
pub const fn lexical_continues() -> bool {
    true
}

pub fn evaluate(spec: Option<&EmbeddingModelSpec>, facts: &ProbeFacts) -> SemanticStatus {
    let Some(spec) = spec else {
        return SemanticStatus::Disabled("embedding_not_configured");
    };
    if !facts.python_available {
        return SemanticStatus::Degraded("python_unavailable");
    }
    match spec.backend {
        // The local fake backend is synthetic: it needs neither a model cache
        // nor a reachable server, only a working Python for the probe contract.
        EmbeddingBackend::Fake => SemanticStatus::Available,
        // An Ollama model lives on the server, so a local model cache is not a
        // requirement. Only the backend reachability gates this branch.
        EmbeddingBackend::Ollama => {
            if !facts.backend_available {
                SemanticStatus::Degraded("ollama_unreachable")
            } else {
                SemanticStatus::Available
            }
        }
        EmbeddingBackend::Python => {
            if !facts.model_available {
                SemanticStatus::Degraded("model_unavailable")
            } else if !facts.backend_available {
                SemanticStatus::Degraded("backend_unavailable")
            } else {
                SemanticStatus::Available
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::semantic::model::{EmbeddingBackend, Normalization};

    fn spec(backend: EmbeddingBackend) -> EmbeddingModelSpec {
        EmbeddingModelSpec {
            model_id: "bge-small-zh-v1.5".into(),
            dimension: 512,
            normalization: Normalization::L2,
            backend,
            model_path: None,
            endpoint: None,
        }
    }

    #[test]
    fn no_spec_is_explicitly_disabled() {
        let status = evaluate(None, &ProbeFacts::all_available());
        assert_eq!(status.as_str(), "disabled");
        assert_eq!(status.reason(), Some("embedding_not_configured"));
        assert!(lexical_continues());
    }

    #[test]
    fn every_missing_capability_degrades_explicitly_and_keeps_lexical() {
        let python_missing = ProbeFacts {
            python_available: false,
            ..ProbeFacts::all_available()
        };
        let status = evaluate(Some(&spec(EmbeddingBackend::Python)), &python_missing);
        assert_eq!(status.as_str(), "degraded");
        assert_eq!(status.reason(), Some("python_unavailable"));
        assert!(lexical_continues());

        let model_missing = ProbeFacts {
            python_available: true,
            model_available: false,
            backend_available: true,
        };
        let status = evaluate(Some(&spec(EmbeddingBackend::Python)), &model_missing);
        assert_eq!(status.as_str(), "degraded");
        assert_eq!(status.reason(), Some("model_unavailable"));
        assert!(lexical_continues());

        let ollama_missing = ProbeFacts {
            python_available: true,
            model_available: true,
            backend_available: false,
        };
        let status = evaluate(Some(&spec(EmbeddingBackend::Ollama)), &ollama_missing);
        assert_eq!(status.as_str(), "degraded");
        assert_eq!(status.reason(), Some("ollama_unreachable"));
        assert!(lexical_continues());
    }

    #[test]
    fn ollama_does_not_require_a_local_model_cache() {
        let facts = ProbeFacts {
            python_available: true,
            model_available: false,
            backend_available: true,
        };
        let status = evaluate(Some(&spec(EmbeddingBackend::Ollama)), &facts);
        assert_eq!(status, SemanticStatus::Available);
        assert!(status.semantic_available());
    }

    #[test]
    fn ollama_unreachable_wins_over_a_missing_local_cache() {
        let facts = ProbeFacts {
            python_available: true,
            model_available: false,
            backend_available: false,
        };
        let status = evaluate(Some(&spec(EmbeddingBackend::Ollama)), &facts);
        assert_eq!(status.reason(), Some("ollama_unreachable"));
    }

    #[test]
    fn python_backend_without_its_dependency_is_also_explicit() {
        let facts = ProbeFacts {
            python_available: true,
            model_available: true,
            backend_available: false,
        };
        let status = evaluate(Some(&spec(EmbeddingBackend::Python)), &facts);
        assert_eq!(status.reason(), Some("backend_unavailable"));
    }

    #[test]
    fn only_all_available_is_available() {
        let status = evaluate(
            Some(&spec(EmbeddingBackend::Python)),
            &ProbeFacts::all_available(),
        );
        assert_eq!(status, SemanticStatus::Available);
        assert!(status.semantic_available());
        assert_eq!(status.reason(), None);
    }
}
