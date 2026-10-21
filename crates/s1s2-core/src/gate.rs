//! Metacognitive gate: the switch between System 1 and System 2 (spec §1, §4.2.4).
//!
//! The gate is deliberately domain-agnostic. It turns four signals — an explicit
//! System 2 request, a saturation reading, a confidence score, and a candidate count —
//! into one ordered verdict, using the priority chain Extension #1 is specified with:
//!
//! 1. `ExplicitSystem2Request`   (user/harness forced compilation)
//! 2. `ContextSaturationWarning` (approaching the hard ceiling; compact before compiling)
//! 3. `LowConfidence`            (System 1 is not sure)
//! 4. `AmbiguousCandidates`      (System 1 is sure *that* it cannot disambiguate)
//! 5. Otherwise commit.

use crate::domain::EscalationReason;

/// Confidence at or above which System 1 is allowed to commit.
pub const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.85;

#[derive(Debug, Clone, PartialEq)]
pub enum SwitchOutcome {
    Commit,
    Escalate(EscalationReason),
}

#[derive(Debug, Clone, Copy)]
pub struct SwitchSignals {
    /// The harness explicitly asked for the deliberative path (`--compile`, MCP arg).
    pub force_system2: bool,
    /// System 1's self-reported confidence, `0.0..=1.0`.
    pub confidence: f32,
    /// Number of plausible System 1 candidates (0 and >1 are both ambiguous).
    pub candidate_count: usize,
    /// Raw saturation ratio from the budgeter.
    pub saturation_ratio: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct MetacognitiveGate {
    pub confidence_threshold: f32,
    pub soft_warning_ratio: f32,
}

impl Default for MetacognitiveGate {
    fn default() -> Self {
        Self {
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            soft_warning_ratio: crate::s1::circuit::DEFAULT_SOFT_WARNING_RATIO,
        }
    }
}

impl MetacognitiveGate {
    pub fn new(confidence_threshold: f32, soft_warning_ratio: f32) -> Self {
        Self { confidence_threshold, soft_warning_ratio }
    }

    pub fn evaluate(&self, signals: SwitchSignals) -> SwitchOutcome {
        if signals.force_system2 {
            return SwitchOutcome::Escalate(EscalationReason::ExplicitSystem2Request);
        }
        if signals.saturation_ratio >= self.soft_warning_ratio {
            return SwitchOutcome::Escalate(EscalationReason::ContextSaturationWarning {
                ratio: signals.saturation_ratio,
            });
        }
        if signals.confidence < self.confidence_threshold {
            return SwitchOutcome::Escalate(EscalationReason::LowConfidence {
                score: signals.confidence,
                threshold: self.confidence_threshold,
            });
        }
        if signals.candidate_count != 1 {
            return SwitchOutcome::Escalate(EscalationReason::AmbiguousCandidates {
                candidate_count: signals.candidate_count,
            });
        }
        SwitchOutcome::Commit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(confidence: f32, candidates: usize, saturation: f32) -> SwitchSignals {
        SwitchSignals {
            force_system2: false,
            confidence,
            candidate_count: candidates,
            saturation_ratio: saturation,
        }
    }

    #[test]
    fn deterministic_single_candidate_commits() {
        let gate = MetacognitiveGate::default();
        assert_eq!(gate.evaluate(signals(0.9, 1, 0.1)), SwitchOutcome::Commit);
        // Exactly at the threshold still commits (>= is deterministic, not "greater than").
        assert_eq!(gate.evaluate(signals(0.85, 1, 0.29)), SwitchOutcome::Commit);
    }

    #[test]
    fn explicit_request_beats_everything() {
        let gate = MetacognitiveGate::default();
        let mut s = signals(0.99, 1, 0.01);
        s.force_system2 = true;
        assert_eq!(gate.evaluate(s), SwitchOutcome::Escalate(EscalationReason::ExplicitSystem2Request));
    }

    #[test]
    fn saturation_warning_precedes_confidence() {
        let gate = MetacognitiveGate::default();
        // 30% soft warning, low confidence: saturation must win so S2 compacts first.
        assert_eq!(
            gate.evaluate(signals(0.2, 0, 0.30)),
            SwitchOutcome::Escalate(EscalationReason::ContextSaturationWarning { ratio: 0.30 })
        );
        assert_eq!(gate.evaluate(signals(0.2, 0, 0.299)), SwitchOutcome::Escalate(
            EscalationReason::LowConfidence { score: 0.2, threshold: 0.85 }
        ));
    }

    #[test]
    fn ambiguity_reports_candidate_count_when_confident_but_multiple() {
        let gate = MetacognitiveGate::default();
        assert_eq!(
            gate.evaluate(signals(0.9, 2, 0.1)),
            SwitchOutcome::Escalate(EscalationReason::AmbiguousCandidates { candidate_count: 2 })
        );
    }

    #[test]
    fn reasons_are_display_slugs_for_xml() {
        assert_eq!(
            EscalationReason::LowConfidence { score: 0.5, threshold: 0.85 }.to_string(),
            "low-confidence:0.500/0.850"
        );
        assert_eq!(EscalationReason::ExplicitSystem2Request.to_string(), "explicit-system2-request");
    }
}
