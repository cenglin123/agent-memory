//! Archive relevance thresholds (P3 rework v4).
//!
//! Round-2 put an absolute semantic floor at candidate generation and a lexical
//! coverage gate before candidates. Held-out data showed both were too strict:
//! correct paraphrases/cross-lingual targets can score as low as 0.53 while
//! unrelated queries reach 0.55–0.65, so an absolute floor cannot separate them;
//! long colloquial queries also have naturally low CJK bigram coverage.
//!
//! Round 3 therefore generates candidates exactly like P1/round 1 and applies a
//! **post-fusion acceptance gate** only when the semantic path is available. A
//! final hit survives when any of these holds:
//!
//! a. its record (or any record in its turn) has cosine >= 0.64;
//! b. its lexical coverage >= 0.34 with >= 2 matched terms (one-term queries are
//!    exempt);
//! c. its turn has >= 2 distinct records with cosine >= 0.50 and turn max >= 0.60.
//!
//! When the semantic path is unavailable the entire gate is skipped and lexical
//! behavior is exactly P1. The gate constants were calibrated on the development
//! set with `qwen3-embedding:4b` at dim 256 (round 3); only `LEXICAL_BONUS` was
//! calibrated at dim 512. Re-calibrate the gate when the model or dimension
//! changes; never calibrate against held-out sets.
//! Round 4 (rework v4) keeps the gate and every threshold above unchanged but
//! makes the semantic cosine the ranking source when it is available:
//! `score = semantic_cosine + LEXICAL_BONUS * coverage` (plus the same tiny
//! turn tie-break). Lexical evidence is therefore a bounded bonus, never a
//! co-equal (or higher) ranking source; a pure lexical candidate uses 0 when its
//! record has no vector. The lexical-only path (degraded / `--mode none`) keeps
//! P1 behavior exactly.
//!
//! Threshold versions and the fusion identity are recorded in the index and
//! status output.

pub const ARCHIVE_RELEVANCE_VERSION: &str = "archive-relevance-v4";

/// Fusion identity written to `detail` and the index.
pub const SCORE_FUSION: &str = "semantic_cosine_lexical_bonus";

/// Bounded lexical contribution to the semantic-dominant score. Calibrated on
/// the development set (`qwen3-embedding:4b`, dim 512) only; see
/// HYBRID-P3-COMPLETED §13 for the full grid.
pub const LEXICAL_BONUS: f64 = 0.0;

pub const SEMANTIC_ACCEPT_SIMILARITY: f64 = 0.64;
pub const SEMANTIC_SUPPORT_SIMILARITY: f64 = 0.50;
pub const SEMANTIC_SUPPORT_UNITS: usize = 2;
pub const SEMANTIC_TURN_MAX_SIMILARITY: f64 = 0.60;

/// Minimum lexical coverage and distinct terms for acceptance rule (b).
pub const LEXICAL_ACCEPT_COVERAGE: f64 = 0.34;
pub const LEXICAL_ACCEPT_DISTINCT: usize = 2;

/// Tiny tie-break bonus for a turn with several matched records.
pub const TURN_AGGREGATION_BONUS: f64 = 0.001;
pub const SEMANTIC_MAX_CANDIDATES: usize = 200;
