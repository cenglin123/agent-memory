//! Frozen P0 evaluation thresholds.
//!
//! The numbers are part of the contract, not a tuning knob: P3/P5 may change
//! them only in a checkpoint with a recorded reason. `docs/verification/
//! HYBRID-P0-METRICS.md` is the human-readable binding document and the
//! documentation-freeze test reads it back.

/// Held-out recall@5 must be at least this value.
pub const RECALL_AT_5_MIN: f64 = 0.80;
/// Correct first-hit turn rate must be at least this value.
pub const FIRST_HIT_TURN_ACCURACY_MIN: f64 = 0.90;
/// Readback must succeed for every returned ref.
pub const READBACK_SUCCESS_RATE_MIN: f64 = 1.0;
/// p95 single-source search latency ceiling, in milliseconds.
pub const P95_LATENCY_MAX_MS: u64 = 20_000;
/// Retrieval index size ceiling relative to the source text.
pub const INDEX_SIZE_MAX_SOURCE_RATIO: f64 = 3.0;
/// Quantized (f16/int8) vectors are allowed inside the size budget.
pub const QUANTIZATION_ALLOWED: bool = true;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ObservedMetrics {
    pub recall_at_5: f64,
    pub first_hit_turn_accuracy: f64,
    pub readback_success_rate: f64,
    pub p95_latency_ms: u64,
    pub index_size_ratio: f64,
}

impl ObservedMetrics {
    pub fn meets_frozen_baseline(&self) -> bool {
        self.recall_at_5 >= RECALL_AT_5_MIN
            && self.first_hit_turn_accuracy >= FIRST_HIT_TURN_ACCURACY_MIN
            && self.readback_success_rate >= READBACK_SUCCESS_RATE_MIN
            && self.p95_latency_ms <= P95_LATENCY_MAX_MS
            && self.index_size_ratio <= INDEX_SIZE_MAX_SOURCE_RATIO
    }
}

/// A borrowed, serializable view useful in reports and checkpoints.
#[derive(Debug, Clone, Copy)]
pub struct FrozenMetrics;

impl FrozenMetrics {
    pub const RECALL_AT_5: f64 = RECALL_AT_5_MIN;
    pub const FIRST_HIT_TURN_ACCURACY: f64 = FIRST_HIT_TURN_ACCURACY_MIN;
    pub const READBACK_SUCCESS_RATE: f64 = READBACK_SUCCESS_RATE_MIN;
    pub const P95_LATENCY_MS: u64 = P95_LATENCY_MAX_MS;
    pub const INDEX_SIZE_MAX_RATIO: f64 = INDEX_SIZE_MAX_SOURCE_RATIO;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_the_frozen_numbers() {
        assert_eq!(FrozenMetrics::RECALL_AT_5, 0.80);
        assert_eq!(FrozenMetrics::FIRST_HIT_TURN_ACCURACY, 0.90);
        assert_eq!(FrozenMetrics::READBACK_SUCCESS_RATE, 1.0);
        assert_eq!(FrozenMetrics::P95_LATENCY_MS, 20_000);
        assert_eq!(FrozenMetrics::INDEX_SIZE_MAX_RATIO, 3.0);
        assert!(QUANTIZATION_ALLOWED);
    }

    #[test]
    fn metrics_document_matches_the_frozen_constants() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/verification/HYBRID-P0-METRICS.md"
        );
        let doc = std::fs::read_to_string(path).expect("metrics document");
        for token in ["recall@5", "0.80", "0.90", "1.00", "20000", "3.0"] {
            assert!(doc.contains(token), "metrics document is missing {token}");
        }
    }

    #[test]
    fn baseline_accepts_exact_boundaries_and_rejects_shortfalls() {
        let exact = ObservedMetrics {
            recall_at_5: 0.80,
            first_hit_turn_accuracy: 0.90,
            readback_success_rate: 1.0,
            p95_latency_ms: 20_000,
            index_size_ratio: 3.0,
        };
        assert!(exact.meets_frozen_baseline());

        for shortfall in [
            ObservedMetrics {
                recall_at_5: 0.799,
                ..exact
            },
            ObservedMetrics {
                first_hit_turn_accuracy: 0.899,
                ..exact
            },
            ObservedMetrics {
                readback_success_rate: 0.999,
                ..exact
            },
            ObservedMetrics {
                p95_latency_ms: 20_001,
                ..exact
            },
            ObservedMetrics {
                index_size_ratio: 3.001,
                ..exact
            },
        ] {
            assert!(!shortfall.meets_frozen_baseline());
        }
    }
}
