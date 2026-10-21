//! Core trait contracts for a cognitive domain (spec §3.1).
//!
//! Associated types keep dispatch static and zero-cost: a domain hands the engine
//! its input payload, registry state, fast-path candidate, final decision, and
//! configuration as concrete types, so no `dyn` objects or allocations are needed.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt;

/// Why System 1 declined to commit and System 2 must take over.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EscalationReason {
    LowConfidence { score: f32, threshold: f32 },
    AmbiguousCandidates { candidate_count: usize },
    ContextSaturationWarning { ratio: f32 },
    ExplicitSystem2Request,
    Custom(String),
}

/// Hard invariant violated by the circuit breaker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CircuitTrip {
    SaturationExceeded { current_ratio: f32, max_ratio: f32 },
    MissingCapabilities,
    InvariantViolation(String),
}

impl fmt::Display for EscalationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EscalationReason::LowConfidence { score, threshold } => {
                write!(f, "low-confidence:{score:.3}/{threshold:.3}")
            }
            EscalationReason::AmbiguousCandidates { candidate_count } => {
                write!(f, "ambiguous-candidates:{candidate_count}")
            }
            EscalationReason::ContextSaturationWarning { ratio } => {
                write!(f, "context-saturation-warning:{ratio:.3}")
            }
            EscalationReason::ExplicitSystem2Request => write!(f, "explicit-system2-request"),
            EscalationReason::Custom(slug) => write!(f, "custom:{slug}"),
        }
    }
}

impl fmt::Display for CircuitTrip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CircuitTrip::SaturationExceeded { current_ratio, max_ratio } => {
                write!(f, "saturation-exceeded:{current_ratio:.3}>{max_ratio:.3}")
            }
            CircuitTrip::MissingCapabilities => write!(f, "missing-capabilities"),
            CircuitTrip::InvariantViolation(slug) => write!(f, "invariant-violation:{slug}"),
        }
    }
}

/// The metacognitive switch's strict three-way output.
#[derive(Debug, Clone, PartialEq)]
pub enum GateVerdict<FastOut> {
    FastCommit(FastOut),
    Escalate {
        partial: Option<FastOut>,
        reason: EscalationReason,
    },
    Halt(CircuitTrip),
}

/// Everything an extension must provide to run on the engine.
pub trait CognitiveDomain: Sized + Send + Sync + 'static {
    /// Domain-specific input payload (e.g. `PromptInput`).
    type Input: fmt::Debug + Clone + DeserializeOwned + Serialize;
    /// Cached environment/registry state (e.g. `BTreeMap<String, AgentEntry>`).
    type RegistryState: fmt::Debug + Clone + Serialize + DeserializeOwned + Default;
    /// Fast-path candidate output (e.g. `HeuristicCandidate`).
    type FastOutput: fmt::Debug + Clone + Serialize;
    /// Final unified decision output (e.g. `RoutingDecision`).
    type FinalDecision: fmt::Debug + Clone + Serialize + DeserializeOwned;
    /// Domain configuration.
    type Config: fmt::Debug + Clone;

    /// 1. Discover and build `RegistryState` when the mtime hash invalidates.
    fn build_registry(config: &Self::Config) -> Result<Self::RegistryState, String>;

    /// 2. Estimate token/resource load for the circuit breaker: `(used, limit)`.
    fn measure_budget(input: &Self::Input, config: &Self::Config) -> (usize, usize);

    /// 3. System 1: pure deterministic/heuristic evaluation, no I/O, `< 5ms`.
    fn execute_system1(
        input: &Self::Input,
        registry: &Self::RegistryState,
        config: &Self::Config,
    ) -> Result<Self::FastOutput, String>;

    /// 4. Metacognitive gate: does System 1 suffice, escalate, or halt?
    ///    The breaker is passed in so a domain can consult hard limits without
    ///    re-deriving them.
    fn evaluate_gate(
        input: &Self::Input,
        s1_out: Self::FastOutput,
        saturation_ratio: f32,
        config: &Self::Config,
        breaker: &crate::s1::circuit::CircuitBreaker,
    ) -> GateVerdict<Self::FinalDecision>;

    /// 5. System 2: deliberative compilation, XML structuring, fallback resolution.
    fn execute_system2(
        input: &Self::Input,
        registry: &Self::RegistryState,
        partial_decision: Option<Self::FinalDecision>,
        reason: &EscalationReason,
        config: &Self::Config,
    ) -> Result<Self::FinalDecision, String>;

    /// 6. Circuit-breaker recovery: safe compaction/handoff decision on halt.
    fn handle_circuit_trip(
        input: &Self::Input,
        trip: CircuitTrip,
        config: &Self::Config,
    ) -> Result<Self::FinalDecision, String>;

    /// Record which pathway produced `decision`, for harnesses that read it off the
    /// decision payload rather than the envelope.
    fn annotate_pathway(
        decision: &mut Self::FinalDecision,
        pathway: &crate::ExecutionPathway,
    ) {
        let _ = (decision, pathway);
    }

    /// Stable identity used for the on-disk cache filename.
    fn domain_id() -> String {
        let name = std::any::type_name::<Self>();
        let stem = name.rsplit("::").next().unwrap_or(name);
        stem.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect()
    }

    /// Directories whose contents define the registry; fingerprinted by `MtimeCache`.
    fn watched_dirs(config: &Self::Config) -> Vec<std::path::PathBuf> {
        let _ = config;
        Vec::new()
    }

    /// Where the serialized registry snapshot lives. Defaults to the OS temp dir so a
    /// domain with no workspace still gets a usable cache.
    fn cache_file(config: &Self::Config) -> std::path::PathBuf {
        let _ = config;
        std::env::temp_dir()
            .join("s1s2-cache")
            .join(format!("{}.json", Self::domain_id()))
    }

    /// File extensions `build_registry` consumes. Filtered by `MtimeCache` so unrelated
    /// files touching a watched directory do not invalidate the snapshot.
    fn registry_extensions() -> Vec<&'static str> {
        vec!["md"]
    }

    /// JSON key holding the payload inside the cache file. Legacy harnesses read a
    /// named field rather than a generic `payload`, so this stays configurable.
    fn cache_payload_key() -> &'static str {
        "payload"
    }
}
