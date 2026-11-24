//! Semantic path: frozen model identity (P0), embedding execution (P3),
//! retrieval-unit projection and the derived vector generation (P3).
//!
//! The Rust core never loads a model. `embed` runs the configured Python helper
//! (or the deterministic test-only fake backend), `units` projects provider
//! records into retrieval units, and `index` owns the vector artifact layout,
//! generation/access validation and rank fusion. An unavailable model is an
//! explicit degradation: lexical recall always stays available.

pub mod degrade;
pub mod embed;
pub mod index;
pub mod metrics;
pub mod model;
pub mod relevance;
pub mod units;

pub use degrade::{evaluate, lexical_continues, ProbeFacts, SemanticStatus};
pub use embed::{EmbeddingPlan, SemanticQuery, SemanticState};
pub use index::{SemanticIndex, SemanticIndexStatus, RRF_K};
pub use relevance::ARCHIVE_RELEVANCE_VERSION;
pub use metrics::{
    FrozenMetrics, ObservedMetrics, FIRST_HIT_TURN_ACCURACY_MIN, INDEX_SIZE_MAX_SOURCE_RATIO,
    P95_LATENCY_MAX_MS, QUANTIZATION_ALLOWED, READBACK_SUCCESS_RATE_MIN, RECALL_AT_5_MIN,
};
pub use model::{EmbeddingBackend, EmbeddingModelSpec, Normalization};
