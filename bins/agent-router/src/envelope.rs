//! Harness wire formats (spec §5.1) — the v0.3.0 stdout/`ROUTED_*` contract, reproduced
//! exactly and extended with v2 pathway metadata.
//!
//! Kept in the binary rather than the extension because the envelope shape is a *harness*
//! concern: the domain returns a `RoutingDecision`, and each harness decides how to present
//! it. Anything that reads `session_meta`/`routing`/`payload` today keeps working.

use ext_agent_router::types::RoutingDecision;
use s1s2_core::harness::cli::SpawnRequest;
use s1s2_core::s2::handoff::SessionLedger;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RouterEnvelope {
    pub session_meta: SessionMeta,
    pub routing: RoutingTarget,
    pub payload: PromptPayload,
    /// v2 addition: pathway, latency, cache status. Absent from the legacy payload, and
    /// additive keys are ignored by existing strict-mode parsers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cognitive: Option<CognitiveMeta>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionMeta {
    pub session_id: u64,
    pub saturation_ratio: f64,
    pub threshold_exceeded: bool,
    pub action: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RoutingTarget {
    pub passthrough: bool,
    pub target_subagent: String,
    pub target_subagent_path: String,
    /// Manifest paths of the required skills, exactly as v0.3.0 emitted them.
    pub required_skills: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PromptPayload {
    pub context_handoff_summary: Option<String>,
    pub compiled_prompt: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CognitiveMeta {
    pub pathway: String,
    pub escalation: Option<String>,
    pub latency_micros: u128,
    pub cache_hit: bool,
}

impl CognitiveMeta {
    fn slug(pathway: &s1s2_core::ExecutionPathway) -> String {
        match pathway {
            s1s2_core::ExecutionPathway::System1Fast => "system1_fast".to_string(),
            s1s2_core::ExecutionPathway::CircuitHalted(trip) => {
                format!("circuit_breaker_halt:{trip}")
            }
            s1s2_core::ExecutionPathway::System2Escalated(reason) => {
                format!("system2_deliberative:{reason}")
            }
        }
    }

    fn escalation(pathway: &s1s2_core::ExecutionPathway) -> Option<String> {
        match pathway {
            s1s2_core::ExecutionPathway::System2Escalated(reason) => Some(reason.to_string()),
            s1s2_core::ExecutionPathway::CircuitHalted(trip) => Some(trip.to_string()),
            s1s2_core::ExecutionPathway::System1Fast => None,
        }
    }
}

/// Assemble the legacy three-section envelope from a domain decision.
pub fn build_envelope(
    ledger: &SessionLedger,
    decision: &RoutingDecision,
    raw_saturation: f32,
    pathway: &s1s2_core::ExecutionPathway,
    latency_micros: u128,
    cache_hit: bool,
) -> RouterEnvelope {
    RouterEnvelope {
        session_meta: SessionMeta {
            session_id: ledger.session_id,
            saturation_ratio: ((raw_saturation as f64) * 1000.0).round() / 1000.0,
            threshold_exceeded: decision.circuit_tripped,
            action: decision.action.clone(),
        },
        routing: RoutingTarget {
            passthrough: decision.passthrough,
            target_subagent: decision.selected_agent.clone(),
            target_subagent_path: decision.target_subagent_path.clone(),
            required_skills: decision.required_skill_paths.clone(),
        },
        payload: PromptPayload {
            context_handoff_summary: decision.handoff_summary.clone(),
            compiled_prompt: decision.compiled_prompt.clone(),
        },
        cognitive: Some(CognitiveMeta {
            pathway: CognitiveMeta::slug(pathway),
            escalation: CognitiveMeta::escalation(pathway),
            latency_micros,
            cache_hit,
        }),
    }
}

/// The `ROUTED_*` block handed to a wrapped harness binary (Mode A).
pub fn routed_env(envelope: &RouterEnvelope) -> Vec<(String, String)> {
    let mut pairs = vec![
        ("ROUTED_SESSION_ID".to_string(), envelope.session_meta.session_id.to_string()),
        ("ROUTED_SESSION_ACTION".to_string(), envelope.session_meta.action.clone()),
        ("ROUTED_SUBAGENT".to_string(), envelope.routing.target_subagent.clone()),
        ("ROUTED_SUBAGENT_PATH".to_string(), envelope.routing.target_subagent_path.clone()),
        ("ROUTED_SKILLS".to_string(), envelope.routing.required_skills.join(",")),
    ];
    if let Some(cognitive) = &envelope.cognitive {
        pairs.push(("ROUTED_PATHWAY".to_string(), cognitive.pathway.clone()));
    }
    pairs
}

/// Final prompt for a wrapped harness: a blocked turn prepends the handoff block.
pub fn final_prompt(envelope: &RouterEnvelope) -> String {
    match &envelope.payload.context_handoff_summary {
        Some(handoff) if envelope.session_meta.threshold_exceeded => format!(
            "<session_handoff>\n{}\n</session_handoff>\n\n{}",
            handoff, envelope.payload.compiled_prompt
        ),
        _ => envelope.payload.compiled_prompt.clone(),
    }
}

/// Mode B: pretty JSON on stdout, nothing else.
pub fn emit_json(envelope: &RouterEnvelope) -> Result<(), String> {
    s1s2_core::harness::cli::emit_json(envelope)
}

/// The envelope as a JSON value, for transports that embed it (MCP text content).
pub fn to_value(envelope: &RouterEnvelope) -> Result<Value, String> {
    serde_json::to_value(envelope).map_err(|e| e.to_string())
}

/// Build the Mode A spawn request: compiled prompt + `ROUTED_*` block + child args.
pub fn spawn_request(program: &str, envelope: &RouterEnvelope, args: &[String]) -> SpawnRequest {
    let mut request = SpawnRequest::new(program, &final_prompt(envelope));
    for arg in args {
        request = request.arg(arg);
    }
    for (key, value) in routed_env(envelope) {
        request = request.env(&key, &value);
    }
    request
}
