//! Generalized token/resource budgeter and saturation circuit breaker (spec §3.2.1).
//!
//! The estimator is the legacy calibrated heuristic, kept byte-for-byte identical so
//! every existing saturation assertion (35% ceiling, handoff sizing, banner percentages)
//! produces the same numbers after the refactor.

use crate::domain::CircuitTrip;

/// Hard ceiling: never let a turn project above 35% of the model context window.
pub const DEFAULT_HARD_CEILING_RATIO: f32 = 0.35;
/// Soft warning: escalate to System 2 so it can compact before the ceiling is hit.
pub const DEFAULT_SOFT_WARNING_RATIO: f32 = 0.30;
/// Window assumed when a caller supplies no limit.
pub const DEFAULT_CONTEXT_WINDOW: usize = 200_000;

/// Zero-dependency token estimator calibrated for code + English
/// (~3.6 chars/token plus a punctuation/newline weighting). No BPE table, no network.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let char_count = text.chars().count();
    let base_tokens = (char_count as f64 / 3.6).ceil() as usize;
    let punct_or_op = text
        .chars()
        .filter(|c| c.is_ascii_punctuation() || *c == '\n')
        .count();
    base_tokens + (punct_or_op / 3)
}

/// Round a saturation ratio the way the legacy envelope reported it (3 decimals).
pub fn round_ratio(ratio: f32) -> f32 {
    (ratio * 1000.0).round() / 1000.0
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CircuitBreakerConfig {
    pub max_saturation_ratio: f32,
    pub soft_warning_ratio: f32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_saturation_ratio: DEFAULT_HARD_CEILING_RATIO,
            soft_warning_ratio: DEFAULT_SOFT_WARNING_RATIO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetReading {
    pub used: usize,
    pub limit: usize,
    /// `used / limit`, clamped to a sane floor when the limit is missing.
    pub saturation_ratio: f32,
}

impl BudgetReading {
    pub fn exceeds_hard_ceiling(&self, cfg: CircuitBreakerConfig) -> bool {
        self.saturation_ratio > cfg.max_saturation_ratio
    }

    pub fn exceeds_soft_warning(&self, cfg: CircuitBreakerConfig) -> bool {
        self.saturation_ratio >= cfg.soft_warning_ratio
    }
}

/// Reusable guard evaluated before System 1 runs, so a saturated context never
/// reaches the (expensive) deliberative path.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreaker {
    pub config: CircuitBreakerConfig,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self { config: CircuitBreakerConfig::default() }
    }
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self { config }
    }

    pub fn read(&self, used: usize, limit: usize) -> BudgetReading {
        let limit = if limit == 0 { DEFAULT_CONTEXT_WINDOW } else { limit };
        BudgetReading {
            used,
            limit,
            saturation_ratio: (used as f32) / (limit as f32),
        }
    }

    /// `Some(trip)` when a hard invariant is violated.
    pub fn trip(&self, reading: &BudgetReading) -> Option<CircuitTrip> {
        if reading.exceeds_hard_ceiling(self.config) {
            Some(CircuitTrip::SaturationExceeded {
                current_ratio: reading.saturation_ratio,
                max_ratio: self.config.max_saturation_ratio,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimator_counts_punctuation_weight() {
        assert_eq!(estimate_tokens(""), 0);
        // 36 chars -> ceil(10) = 10 tokens, no punctuation.
        assert_eq!(estimate_tokens(&"a".repeat(36)), 10);
        let with_punct = "a\n".repeat(18); // 36 chars, 18 newlines
        assert_eq!(estimate_tokens(&with_punct), 10 + 6);
    }

    #[test]
    fn breaker_trips_strictly_above_the_ceiling() {
        let breaker = CircuitBreaker::default();
        let exactly = breaker.read(70_000, 200_000);
        assert_eq!(exactly.saturation_ratio, 0.35);
        assert!(breaker.trip(&exactly).is_none(), "0.35 is not > 0.35");

        let over = breaker.read(75_000, 200_000);
        assert_eq!(over.saturation_ratio, 0.375);
        assert_eq!(
            breaker.trip(&over),
            Some(CircuitTrip::SaturationExceeded { current_ratio: 0.375, max_ratio: 0.35 })
        );
    }

    #[test]
    fn soft_warning_fires_before_the_ceiling() {
        let breaker = CircuitBreaker::default();
        let warn = breaker.read(62_000, 200_000);
        assert!(warn.exceeds_soft_warning(breaker.config));
        assert!(!warn.exceeds_hard_ceiling(breaker.config));
        let clean = breaker.read(20_000, 200_000);
        assert_eq!(clean.saturation_ratio, 0.1);
        assert!(!clean.exceeds_soft_warning(breaker.config));
    }

    #[test]
    fn zero_limit_falls_back_to_the_default_window() {
        let breaker = CircuitBreaker::default();
        let reading = breaker.read(20_000, 0);
        assert_eq!(reading.limit, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn round_ratio_matches_legacy_three_decimal_format() {
        assert_eq!(round_ratio(0.35234375), 0.352);
        assert_eq!(round_ratio(0.375001), 0.375);
    }
}
