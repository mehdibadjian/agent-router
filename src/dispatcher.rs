use crate::budget::BudgetEvaluation;
use crate::types::{
    AgentEntry, LlmCompilerOutput, PromptPayload, RouterEnvelope, RoutingTarget, SessionMeta,
    SessionState,
};
use std::collections::BTreeMap;
use std::env;
use std::process::{Command, Stdio};

pub fn build_envelope(
    session: &SessionState,
    eval: &BudgetEvaluation,
    decision: LlmCompilerOutput,
    registry: &BTreeMap<String, AgentEntry>,
) -> RouterEnvelope {
    let target_subagent_path = registry
        .get(&decision.target_subagent)
        .map(|e| e.path.clone())
        .unwrap_or_else(|| "default".to_string());

    let skill_paths: Vec<String> = decision
        .required_skills
        .iter()
        .filter_map(|id| registry.get(id).map(|e| e.path.clone()))
        .collect();

    RouterEnvelope {
        session_meta: SessionMeta {
            session_id: session.session_id,
            saturation_ratio: (eval.saturation_ratio * 1000.0).round() / 1000.0,
            threshold_exceeded: eval.must_reset_session,
            action: if eval.must_reset_session {
                "START_NEW_SESSION".to_string()
            } else {
                "CONTINUE_SESSION".to_string()
            },
        },
        routing: RoutingTarget {
            passthrough: decision.passthrough,
            target_subagent: decision.target_subagent,
            target_subagent_path,
            required_skills: skill_paths,
        },
        payload: PromptPayload {
            context_handoff_summary: decision.context_handoff_summary,
            compiled_prompt: decision.compiled_prompt,
        },
    }
}

pub fn execute_dispatch(envelope: &RouterEnvelope) -> Result<i32, String> {
    if let Ok(harness_bin) = env::var("ROUTER_HARNESS_BIN") {
        let final_prompt = match &envelope.payload.context_handoff_summary {
            Some(handoff) if envelope.session_meta.threshold_exceeded => format!(
                "<session_handoff>\n{}\n</session_handoff>\n\n{}",
                handoff, envelope.payload.compiled_prompt
            ),
            _ => envelope.payload.compiled_prompt.clone(),
        };

        let status = Command::new(harness_bin)
            .env("ROUTED_SESSION_ID", envelope.session_meta.session_id.to_string())
            .env("ROUTED_SESSION_ACTION", &envelope.session_meta.action)
            .env("ROUTED_SUBAGENT", &envelope.routing.target_subagent)
            .env("ROUTED_SUBAGENT_PATH", &envelope.routing.target_subagent_path)
            .env("ROUTED_SKILLS", envelope.routing.required_skills.join(","))
            .arg(&final_prompt)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to spawn harness binary: {}", e))?;

        Ok(status.code().unwrap_or(0))
    } else {
        let json = serde_json::to_string_pretty(envelope).map_err(|e| e.to_string())?;
        println!("{}", json);
        Ok(0)
    }
}
