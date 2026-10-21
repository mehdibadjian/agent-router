//! Extension #1 — the developer subagent routing domain, expressed as a
//! [`CognitiveDomain`] (spec §4).
//!
//! `AgentRoutingDomain` holds no state: the registry comes from the engine's
//! `MtimeCache`, the session ledger arrives through [`PromptInput::session_state`], and
//! every stage is a pure function of `(input, registry, config)`.

pub mod compiler;
pub mod heuristics;
pub mod registry;
pub mod types;

use compiler::LlmCompilerOutput;
use heuristics::HeuristicCandidate;
use s1s2_core::domain::{CircuitTrip, CognitiveDomain, EscalationReason, GateVerdict};
use s1s2_core::gate::{MetacognitiveGate, SwitchOutcome, SwitchSignals};
use s1s2_core::s1::circuit::{estimate_tokens, round_ratio, CircuitBreaker, DEFAULT_CONTEXT_WINDOW};
use s1s2_core::s2::handoff::{
    render_handoff_artifact, HandoffCause, PathwayKind, SessionLedger,
};
use s1s2_core::s2::xml_compiler::XmlEnvelopeBuilder;
use s1s2_core::ExecutionPathway;
use std::path::PathBuf;
use types::{AgentEntry, AgentRegistry, PromptInput, RouterConfig, RoutingDecision};

/// System 1 confidence at or above which the fast path may commit (spec §4.2.4).
pub const CONFIDENCE_THRESHOLD: f32 = 0.85;

pub struct AgentRoutingDomain;

impl CognitiveDomain for AgentRoutingDomain {
    type Input = PromptInput;
    type RegistryState = AgentRegistry;
    type FastOutput = HeuristicCandidate;
    type FinalDecision = RoutingDecision;
    type Config = RouterConfig;

    fn domain_id() -> String {
        "agent-router".to_string()
    }

    fn watched_dirs(config: &RouterConfig) -> Vec<PathBuf> {
        config
            .search_dirs
            .iter()
            .map(|(dir, _)| config.repo_root.join(dir))
            .collect()
    }

    fn cache_file(config: &RouterConfig) -> PathBuf {
        config.registry_cache_file.clone()
    }

    /// Keeps `.router/registry.json` byte-compatible with v0.3.0.
    fn cache_payload_key() -> &'static str {
        "entries"
    }

    fn registry_extensions() -> Vec<&'static str> {
        vec!["md"]
    }

    fn build_registry(config: &RouterConfig) -> Result<AgentRegistry, String> {
        registry::build_registry(&config.repo_root, &config.search_dirs)
    }

    /// Prompt estimate + committed ledger, against the harness window (spec §4.2.2).
    fn measure_budget(input: &PromptInput, config: &RouterConfig) -> (usize, usize) {
        let used =
            estimate_tokens(&input.raw_prompt) + input.context_tokens_used + config.base_overhead_tokens;
        // An explicit per-request window wins; otherwise the configured harness window is
        // authoritative, falling back to the spec default only when unset.
        let limit = if input.context_window_limit > 0 {
            input.context_window_limit
        } else if config.max_window_tokens > 0 {
            config.max_window_tokens
        } else {
            DEFAULT_CONTEXT_WINDOW
        };
        (used, limit)
    }

    /// Deterministic lexical scoring only: no I/O, no clock, no network.
    fn execute_system1(
        input: &PromptInput,
        registry: &AgentRegistry,
        _config: &RouterConfig,
    ) -> Result<HeuristicCandidate, String> {
        Ok(heuristics::score_candidate(&input.raw_prompt, registry))
    }

    fn evaluate_gate(
        input: &PromptInput,
        s1_out: HeuristicCandidate,
        saturation_ratio: f32,
        config: &RouterConfig,
        breaker: &CircuitBreaker,
    ) -> GateVerdict<RoutingDecision> {
        // A saturated context is a hard invariant, checked here as well as by the engine
        // so the domain can never be coerced into committing over the ceiling.
        if saturation_ratio > breaker.config.max_saturation_ratio {
            return GateVerdict::Halt(CircuitTrip::SaturationExceeded {
                current_ratio: saturation_ratio,
                max_ratio: breaker.config.max_saturation_ratio,
            });
        }

        // A trivial shell/test invocation is already a complete instruction. Committing
        // here preserves the legacy passthrough path and keeps System 2 — and any network
        // adjudicator — away from prompts that need no compilation.
        if !input.force_compile && heuristics::is_trivial_passthrough(&input.raw_prompt) {
            return GateVerdict::FastCommit(commit(
                input,
                s1_out,
                saturation_ratio,
                "default".to_string(),
                None,
                true,
            ));
        }

        let gate = MetacognitiveGate::new(CONFIDENCE_THRESHOLD, config.soft_warning_ratio);
        let signals = SwitchSignals {
            force_system2: input.force_compile,
            confidence: s1_out.confidence,
            candidate_count: s1_out.candidate_count,
            saturation_ratio,
        };

        match gate.evaluate(signals) {
            SwitchOutcome::Commit => {
                let agent = s1_out.matched_agent.clone();
                GateVerdict::FastCommit(commit(
                    input,
                    s1_out,
                    saturation_ratio,
                    agent.as_ref().map(|e| e.id.clone()).unwrap_or_default(),
                    agent,
                    false,
                ))
            }
            SwitchOutcome::Escalate(reason) => {
                let agent = s1_out.matched_agent.clone();
                let partial = commit(
                    input,
                    s1_out,
                    saturation_ratio,
                    agent.as_ref().map(|e| e.id.clone()).unwrap_or_else(|| "default".into()),
                    agent,
                    false,
                );
                GateVerdict::Escalate { partial: Some(partial), reason }
            }
        }
    }

    fn execute_system2(
        input: &PromptInput,
        registry: &AgentRegistry,
        partial_decision: Option<RoutingDecision>,
        reason: &EscalationReason,
        config: &RouterConfig,
    ) -> Result<RoutingDecision, String> {
        let candidate = heuristics::score_candidate(&input.raw_prompt, registry);
        let mut decision = match try_adjudicate(input, registry, config, reason) {
            Some(Ok(output)) => from_adjudicated(input, registry, config, output, &candidate),
            Some(Err(err)) => {
                eprintln!(
                    "[agent-router] LLM compiler unavailable ({err}). Failing open with local heuristic."
                );
                deterministic(input, registry, config, &candidate, reason)
            }
            None => deterministic(input, registry, config, &candidate, reason),
        };

        // Continuity: inherit System 1's session stamp if the compiler left it unset.
        if decision.session_id == 0 {
            if let Some(partial) = partial_decision {
                decision.session_id = partial.session_id;
            }
        }
        Ok(decision)
    }

    fn handle_circuit_trip(
        input: &PromptInput,
        trip: CircuitTrip,
        config: &RouterConfig,
    ) -> Result<RoutingDecision, String> {
        let (used, limit) = Self::measure_budget(input, config);
        let ratio = (used as f32) / (limit.max(1) as f32);
        let ledger = ledger_of(input);
        let max_ratio = config.hard_ceiling_ratio;

        // Machine-readable continuity payload for the next session.
        let artifact = render_handoff_artifact(
            &ledger,
            &HandoffCause::CircuitTripped(trip.clone()),
            ratio,
            max_ratio,
        );
        // Legacy summary line, so existing `.router/HANDOFF.md` readers see the same text.
        let summary = format!(
            "Prior session reset at >35% context ceiling. Previous summary: {}",
            if ledger.rolling_summary.is_empty() { "None recorded." } else { &ledger.rolling_summary }
        );

        // The prompt itself becomes a compaction directive, not a task.
        let mut breaker = XmlEnvelopeBuilder::new();
        breaker.open(
            "circuit_breaker_tripped",
            &[
                ("max", &format!("{:.2}", max_ratio)),
                ("saturation", &format!("{:.3}", ratio)),
                ("action", "START_NEW_SESSION"),
            ],
        );
        breaker.text_tag(
            "compaction_directive",
            &[],
            "Context ceiling exceeded. Stop expanding, compress the working state into the handoff summary, and resume in a fresh session.",
        );
        breaker.text_tag("objective", &[], input.raw_prompt.trim());
        breaker.close("circuit_breaker_tripped");

        Ok(RoutingDecision {
            selected_agent: "default".to_string(),
            target_subagent_path: "default".to_string(),
            required_skills: Vec::new(),
            required_skill_paths: Vec::new(),
            passthrough: false,
            compiled_prompt: breaker.finish(),
            saturation_ratio: round_ratio(ratio),
            circuit_tripped: true,
            action: PathwayKind::CircuitBreakerHalt.legacy_action().to_string(),
            session_id: ledger.session_id,
            handoff_artifact: Some(artifact),
            handoff_summary: Some(summary),
            pathway_metadata: Some(ExecutionPathway::CircuitHalted(trip)),
        })
    }

    fn annotate_pathway(decision: &mut RoutingDecision, pathway: &ExecutionPathway) {
        decision.pathway_metadata = Some(pathway.clone());
    }
}

fn ledger_of(input: &PromptInput) -> SessionLedger {
    input.session_state.clone().unwrap_or_default()
}

/// Fast-path decision: the legacy deterministic template, no deliberative envelope.
fn commit(
    input: &PromptInput,
    candidate: HeuristicCandidate,
    saturation_ratio: f32,
    selected: String,
    selected_entry: Option<AgentEntry>,
    passthrough: bool,
) -> RoutingDecision {
    let ledger = ledger_of(input);
    let compiled_prompt = if passthrough {
        input.raw_prompt.clone()
    } else {
        compiler::compile_deterministic(&input.raw_prompt)
    };

    RoutingDecision {
        selected_agent: if selected.is_empty() { "default".to_string() } else { selected },
        target_subagent_path: selected_entry
            .map(|e| e.path)
            .or_else(|| candidate.best_agent().map(|e| e.path.clone()))
            .unwrap_or_else(|| "default".to_string()),
        required_skills: candidate.skill_ids(),
        required_skill_paths: candidate.skill_paths(),
        passthrough,
        compiled_prompt,
        saturation_ratio: round_ratio(saturation_ratio),
        circuit_tripped: false,
        action: PathwayKind::System1Fast.legacy_action().to_string(),
        session_id: ledger.session_id,
        handoff_artifact: None,
        handoff_summary: None,
        pathway_metadata: Some(ExecutionPathway::System1Fast),
    }
}

/// Local deterministic System 2: XML envelope, capability-matrix tie-break, no network.
fn deterministic(
    input: &PromptInput,
    registry: &AgentRegistry,
    config: &RouterConfig,
    candidate: &HeuristicCandidate,
    reason: &EscalationReason,
) -> RoutingDecision {
    let winner = candidate
        .best_agent()
        .map(|e| e.id.clone())
        .or_else(|| compiler::resolve_ambiguity(&input.raw_prompt, candidate).map(|e| e.id))
        .unwrap_or_else(|| "default".to_string());

    let ledger = ledger_of(input);
    let (used, limit) = AgentRoutingDomain::measure_budget(input, config);
    let ratio = (used as f32) / (limit.max(1) as f32);
    let saturating = ratio >= config.soft_warning_ratio;
    let skills = matched_skills(registry, candidate);
    let handoff = saturating.then(|| compact_summary(&ledger, input));

    let compiled_prompt = compiler::compile_envelope(
        input,
        candidate,
        registry,
        &winner,
        &skills,
        ratio,
        used,
        limit,
        handoff.as_deref(),
    );

    RoutingDecision {
        selected_agent: winner.clone(),
        target_subagent_path: registry
            .get(&winner)
            .map(|e| e.path.clone())
            .unwrap_or_else(|| "default".to_string()),
        required_skills: skills.iter().map(|e| e.id.clone()).collect(),
        required_skill_paths: skills.iter().map(|e| e.path.clone()).collect(),
        passthrough: false,
        compiled_prompt,
        saturation_ratio: round_ratio(ratio),
        circuit_tripped: false,
        action: PathwayKind::System2Deliberative.legacy_action().to_string(),
        session_id: ledger.session_id,
        handoff_artifact: match reason {
            EscalationReason::ContextSaturationWarning { ratio } => Some(render_handoff_artifact(
                &ledger,
                &HandoffCause::Escalated(reason.clone()),
                *ratio,
                config.hard_ceiling_ratio,
            )),
            _ => None,
        },
        handoff_summary: handoff,
        pathway_metadata: Some(ExecutionPathway::System2Escalated(reason.clone())),
    }
}

fn matched_skills(registry: &AgentRegistry, candidate: &HeuristicCandidate) -> Vec<AgentEntry> {
    candidate
        .matched_skills
        .iter()
        .filter_map(|e| registry.get(&e.id))
        .cloned()
        .collect()
}

/// Bound the compacted handoff so it cannot itself breach the ceiling it protects.
fn compact_summary(ledger: &SessionLedger, input: &PromptInput) -> String {
    let prior = if ledger.rolling_summary.is_empty() { "None recorded." } else { ledger.rolling_summary.as_str() };
    let intent: String = input.raw_prompt.trim().chars().take(200).collect();
    format!("{prior} | current intent: {intent}")
}

/// Ask the configured endpoint, if any. `None` means "this domain runs offline".
fn try_adjudicate(
    input: &PromptInput,
    registry: &AgentRegistry,
    config: &RouterConfig,
    reason: &EscalationReason,
) -> Option<Result<LlmCompilerOutput, String>> {
    if !config.llm.enabled {
        return None;
    }
    let saturation_reset = matches!(reason, EscalationReason::ContextSaturationWarning { .. });
    Some(compiler::adjudicate(
        input,
        registry,
        &ledger_of(input).rolling_summary,
        saturation_reset,
        &config.llm,
    ))
}

/// Adjudicator output, sanitized against the registry so invented ids never escape.
fn from_adjudicated(
    input: &PromptInput,
    registry: &AgentRegistry,
    config: &RouterConfig,
    mut output: LlmCompilerOutput,
    candidate: &HeuristicCandidate,
) -> RoutingDecision {
    compiler::sanitize_hallucinated_ids(&mut output, registry);

    let skills: Vec<AgentEntry> = if output.required_skills.is_empty() {
        matched_skills(registry, candidate)
    } else {
        output.required_skills.iter().filter_map(|id| registry.get(id)).cloned().collect()
    };

    let (used, limit) = AgentRoutingDomain::measure_budget(input, config);
    let ratio = (used as f32) / (limit.max(1) as f32);
    let ledger = ledger_of(input);

    let target_subagent = std::mem::take(&mut output.target_subagent);
    let compiled_prompt = std::mem::take(&mut output.compiled_prompt);
    let handoff_summary = output.context_handoff_summary.take();

    RoutingDecision {
        selected_agent: target_subagent.clone(),
        target_subagent_path: registry
            .get(&target_subagent)
            .map(|e| e.path.clone())
            .unwrap_or_else(|| "default".to_string()),
        required_skills: skills.iter().map(|e| e.id.clone()).collect(),
        required_skill_paths: skills.iter().map(|e| e.path.clone()).collect(),
        passthrough: output.passthrough,
        compiled_prompt,
        saturation_ratio: round_ratio(ratio),
        circuit_tripped: false,
        action: PathwayKind::System2Deliberative.legacy_action().to_string(),
        session_id: ledger.session_id,
        handoff_artifact: None,
        handoff_summary,
        pathway_metadata: Some(ExecutionPathway::System2Escalated(EscalationReason::ExplicitSystem2Request)),
    }
}
