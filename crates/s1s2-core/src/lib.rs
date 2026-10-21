//! Reusable System 1 / System 2 cognitive engine (spec §1, §3.4).
//!
//! `CognitiveEngine<D>` is the coordinator: it owns the registry cache and runs the
//! strict pipeline `budget -> System 1 -> gate -> System 2`, timing each turn so callers
//! can assert the fast path really is fast. The engine holds no domain logic — everything
//! policy-related lives in the `CognitiveDomain` implementation.

pub mod domain;
pub mod gate;
pub mod harness;
pub mod s1;
pub mod s2;

use crate::domain::{CircuitTrip, CognitiveDomain, EscalationReason, GateVerdict};
use crate::s1::cache::MtimeCache;
use crate::s1::circuit::{CircuitBreaker, CircuitBreakerConfig};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Which pathway produced a decision, recorded for observability and continuity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExecutionPathway {
    System1Fast,
    System2Escalated(EscalationReason),
    CircuitHalted(CircuitTrip),
}

impl ExecutionPathway {
    /// Collapse the pathway to the coarse kind persisted in session ledgers.
    pub fn kind(&self) -> s2::handoff::PathwayKind {
        match self {
            ExecutionPathway::System1Fast => s2::handoff::PathwayKind::System1Fast,
            ExecutionPathway::System2Escalated(_) => s2::handoff::PathwayKind::System2Deliberative,
            ExecutionPathway::CircuitHalted(_) => s2::handoff::PathwayKind::CircuitBreakerHalt,
        }
    }
}

/// The engine's unified output: the decision plus the metadata explaining how it was reached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitiveEnvelope<Out> {
    pub pathway: ExecutionPathway,
    pub saturation_ratio: f32,
    pub latency_micros: u128,
    pub cache_hit: bool,
    pub decision: Out,
}

/// Coordinator that drives any `CognitiveDomain` through the three-stage pipeline.
pub struct CognitiveEngine<D: CognitiveDomain> {
    pub config: D::Config,
    pub breaker: CircuitBreaker,
    cache: MtimeCache<D::RegistryState>,
}

impl<D: CognitiveDomain> CognitiveEngine<D> {
    pub fn new(config: D::Config, breaker_config: CircuitBreakerConfig) -> Self {
        let cache = MtimeCache::new(
            D::cache_file(&config),
            D::cache_payload_key(),
            &D::registry_extensions(),
        );
        Self { config, breaker: CircuitBreaker::new(breaker_config), cache }
    }

    /// Drop the cached registry so the next turn rebuilds it from source.
    pub fn refresh_registry(&mut self) {
        self.cache.invalidate();
    }

    /// Drop and immediately rebuild the snapshot, so a harness can force a refresh from
    /// disk without running a full cognitive turn.
    pub fn rebuild_registry(&mut self) -> Result<(), String> {
        self.cache.invalidate();
        let watched = D::watched_dirs(&self.config);
        self.cache
            .load_or_build(&watched, |_| D::build_registry(&self.config))?;
        Ok(())
    }

    /// Budget-only probe (exposed to harnesses as `check_context_budget`).
    pub fn budget_check(&self, input: &D::Input) -> (f32, usize, usize, Option<CircuitTrip>) {
        let (used, limit) = D::measure_budget(input, &self.config);
        let reading = self.breaker.read(used, limit);
        (reading.saturation_ratio, used, limit, self.breaker.trip(&reading))
    }

    /// Run the full pipeline: System 1 -> metacognitive gate -> System 2.
    pub fn process(&mut self, input: &D::Input) -> Result<CognitiveEnvelope<D::FinalDecision>, String> {
        let started = Instant::now();

        // Step 2: cached registry state, rebuilt only when the mtime fingerprint moves.
        let watched = D::watched_dirs(&self.config);
        let state = self
            .cache
            .load_or_build(&watched, |_| D::build_registry(&self.config))?;
        let registry = state.payload;
        let cache_hit = state.cache_hit;

        // Step 0: hard invariant check, before System 1 or System 2 can spend tokens.
        let (used, limit) = D::measure_budget(input, &self.config);
        let reading = self.breaker.read(used, limit);
        if let Some(trip) = self.breaker.trip(&reading) {
            let decision = D::handle_circuit_trip(input, trip.clone(), &self.config)?;
            return Ok(self.envelope(
                ExecutionPathway::CircuitHalted(trip),
                reading.saturation_ratio,
                cache_hit,
                decision,
                started,
            ));
        }

        // Step 1: deterministic fast path.
        let fast = D::execute_system1(input, &registry, &self.config)?;

        // The switch.
        match D::evaluate_gate(input, fast, reading.saturation_ratio, &self.config, &self.breaker) {
            GateVerdict::FastCommit(decision) => Ok(self.envelope(
                ExecutionPathway::System1Fast,
                reading.saturation_ratio,
                cache_hit,
                decision,
                started,
            )),
            GateVerdict::Halt(trip) => {
                let decision = D::handle_circuit_trip(input, trip.clone(), &self.config)?;
                Ok(self.envelope(
                    ExecutionPathway::CircuitHalted(trip),
                    reading.saturation_ratio,
                    cache_hit,
                    decision,
                    started,
                ))
            }
            GateVerdict::Escalate { partial, reason } => {
                let decision =
                    D::execute_system2(input, &registry, partial, &reason, &self.config)?;
                Ok(self.envelope(
                    ExecutionPathway::System2Escalated(reason),
                    reading.saturation_ratio,
                    cache_hit,
                    decision,
                    started,
                ))
            }
        }
    }

    fn envelope(
        &self,
        pathway: ExecutionPathway,
        saturation_ratio: f32,
        cache_hit: bool,
        mut decision: D::FinalDecision,
        started: Instant,
    ) -> CognitiveEnvelope<D::FinalDecision> {
        // Stamping the pathway on the decision keeps the payload self-describing for
        // harnesses that only read the decision, never the envelope.
        D::annotate_pathway(&mut decision, &pathway);
        CognitiveEnvelope {
            pathway,
            saturation_ratio,
            latency_micros: started.elapsed().as_micros(),
            cache_hit,
            decision,
        }
    }
}
