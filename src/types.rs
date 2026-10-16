use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentEntry {
    pub id: String,
    pub kind: String, // "agent" or "skill"
    pub path: String,
    pub description: String,
    pub triggers: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RegistryCache {
    pub hash: u64,
    pub entries: BTreeMap<String, AgentEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionState {
    pub session_id: u64,
    pub accumulated_tokens: usize,
    pub rolling_summary: String,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: 1,
            accumulated_tokens: 0,
            rolling_summary: String::new(),
        }
    }
}

/// Raw JSON schema expected back from the low-cost LLM
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LlmCompilerOutput {
    pub passthrough: bool,
    pub target_subagent: String,
    pub required_skills: Vec<String>,
    pub compiled_prompt: String,
    pub context_handoff_summary: Option<String>,
}

/// Final deterministic payload emitted to stdout or MCP client
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RouterEnvelope {
    pub session_meta: SessionMeta,
    pub routing: RoutingTarget,
    pub payload: PromptPayload,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionMeta {
    pub session_id: u64,
    pub saturation_ratio: f64,
    pub threshold_exceeded: bool,
    pub action: String, // "CONTINUE_SESSION" or "START_NEW_SESSION"
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RoutingTarget {
    pub passthrough: bool,
    pub target_subagent: String,
    pub target_subagent_path: String,
    pub required_skills: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PromptPayload {
    pub context_handoff_summary: Option<String>,
    pub compiled_prompt: String,
}
