//! Spec §6 Phase 4, test 5: a minimal non-routing domain must compile and execute
//! cleanly on the framework, proving `s1s2-core` is genuinely reusable.
//!
//! `MockSecurityGateDomain` is deliberately tiny: no registry files, no XML needs
//! beyond a single tag, and only three outcomes (allow / redact / halt).

use s1s2_core::domain::{CircuitTrip, CognitiveDomain, EscalationReason, GateVerdict};
use s1s2_core::{CognitiveEngine, ExecutionPathway};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRequest {
    pub text: String,
    #[serde(default)]
    pub context_tokens_used: usize,
    #[serde(default)]
    pub context_window_limit: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmptyRegistry;

#[derive(Debug, Clone, Serialize)]
pub struct Scan {
    pub banned_hits: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub allow: bool,
    pub note: String,
}

/// A second, unrelated extension: a secrets scanner, not a router.
#[derive(Debug, Clone)]
pub struct MockSecurityGateDomain;

const BANNED: [&str; 2] = ["password", "secret"];

impl CognitiveDomain for MockSecurityGateDomain {
    type Input = SecretRequest;
    type RegistryState = EmptyRegistry;
    type FastOutput = Scan;
    type FinalDecision = Verdict;
    /// Escalation threshold, expressed in banned-term hits.
    type Config = u32;

    fn build_registry(_config: &u32) -> Result<EmptyRegistry, String> {
        Ok(EmptyRegistry)
    }

    fn measure_budget(input: &SecretRequest, _config: &u32) -> (usize, usize) {
        let limit = if input.context_window_limit == 0 {
            200_000
        } else {
            input.context_window_limit
        };
        (input.context_tokens_used, limit)
    }

    fn execute_system1(
        input: &SecretRequest,
        _registry: &EmptyRegistry,
        _config: &u32,
    ) -> Result<Scan, String> {
        let lower = input.text.to_lowercase();
        Ok(Scan {
            banned_hits: BANNED.iter().filter(|b| lower.contains(*b)).count(),
        })
    }

    fn evaluate_gate(
        _input: &SecretRequest,
        s1_out: Scan,
        _saturation_ratio: f32,
        threshold: &u32,
        _breaker: &s1s2_core::s1::circuit::CircuitBreaker,
    ) -> GateVerdict<Verdict> {
        if (s1_out.banned_hits as u32) < *threshold {
            GateVerdict::FastCommit(Verdict {
                allow: true,
                note: "clean".to_string(),
            })
        } else {
            GateVerdict::Escalate {
                partial: None,
                reason: EscalationReason::Custom("banned-term".to_string()),
            }
        }
    }

    fn execute_system2(
        input: &SecretRequest,
        _registry: &EmptyRegistry,
        _partial: Option<Verdict>,
        reason: &EscalationReason,
        _config: &u32,
    ) -> Result<Verdict, String> {
        Ok(Verdict {
            allow: false,
            note: format!(
                "<redaction reason=\"{}\">chars={}</redaction>",
                reason,
                input.text.chars().count()
            ),
        })
    }

    fn handle_circuit_trip(
        _input: &SecretRequest,
        trip: CircuitTrip,
        _config: &u32,
    ) -> Result<Verdict, String> {
        Ok(Verdict {
            allow: false,
            note: format!("<halt trip=\"{trip}\">state preserved</halt>"),
        })
    }
}

fn request(text: &str, used: usize) -> SecretRequest {
    SecretRequest {
        text: text.to_string(),
        context_tokens_used: used,
        context_window_limit: 200_000,
    }
}

#[test]
fn test_dummy_second_extension() {
    // Relaxed threshold (1) => a clean prompt fast-commits without touching System 2.
    let mut engine = CognitiveEngine::<MockSecurityGateDomain>::new(1, Default::default());
    let env = engine
        .process(&request("please sort the vec", 1_000))
        .expect("mock domain must execute");
    assert!(matches!(env.pathway, ExecutionPathway::System1Fast));
    assert!(env.decision.allow);
    assert_eq!(env.decision.note, "clean");
    assert_eq!(env.saturation_ratio, 0.005);
    assert!(env.latency_micros < 5_000, "mock S1 must stay in the <5ms budget");

    // Strict threshold (0) => any banned hit escalates to System 2.
    let mut strict = CognitiveEngine::<MockSecurityGateDomain>::new(0, Default::default());
    let env = strict
        .process(&request("print the password", 1_000))
        .expect("mock domain must escalate");
    match &env.pathway {
        ExecutionPathway::System2Escalated(reason) => {
            assert_eq!(reason, &EscalationReason::Custom("banned-term".to_string()));
        }
        other => panic!("expected escalation, got {other:?}"),
    }
    assert!(!env.decision.allow);
    assert!(
        env.decision.note.starts_with("<redaction reason="),
        "{}",
        env.decision.note
    );

    // 37.5% saturation => hard halt, System 2 never runs.
    let env = engine
        .process(&request("hello", 75_000))
        .expect("mock domain must halt");
    assert!(
        matches!(
            env.pathway,
            ExecutionPathway::CircuitHalted(CircuitTrip::SaturationExceeded { .. })
        ),
        "{:?}",
        env.pathway
    );
    assert!(!env.decision.allow);
    assert!(env.decision.note.starts_with("<halt trip="));
}
