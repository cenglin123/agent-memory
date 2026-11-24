//! Per-source budget (§3.1 / §3.5).

pub const DEFAULT_MAX_BYTES_PER_SOURCE: u64 = 33_554_432;
pub const DEFAULT_MAX_ELAPSED_MS_PER_SOURCE: u64 = 20_000;
pub const DEFAULT_KB_TOP_K: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_bytes_per_source: u64,
    pub max_elapsed_ms_per_source: u64,
    pub kb_top_k: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_bytes_per_source: DEFAULT_MAX_BYTES_PER_SOURCE,
            max_elapsed_ms_per_source: DEFAULT_MAX_ELAPSED_MS_PER_SOURCE,
            kb_top_k: DEFAULT_KB_TOP_K,
        }
    }
}

impl Budget {
    /// `max(1000, min(18000, max_elapsed_ms - 2000))`, always positive and
    /// strictly below the source budget.
    pub fn kb_hard_timeout_ms(&self) -> u64 {
        self.max_elapsed_ms_per_source
            .saturating_sub(2_000)
            .clamp(1_000, 18_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_timeout_bounds() {
        let budget = Budget {
            max_elapsed_ms_per_source: 4_000,
            ..Budget::default()
        };
        assert_eq!(budget.kb_hard_timeout_ms(), 2_000);
        assert_eq!(Budget::default().kb_hard_timeout_ms(), 18_000);
        let budget = Budget {
            max_elapsed_ms_per_source: 600_000,
            ..Budget::default()
        };
        assert_eq!(budget.kb_hard_timeout_ms(), 18_000);
    }
}
