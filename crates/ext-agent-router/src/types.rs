//! Extension #1 domain types (spec §4.1), migrated from the legacy `src/types.rs`.
//!
//! Wire-format compatibility rules this module follows:
//! * Every type keeps the legacy field names and JSON keys. The spec's proposed names
//!   are attached as serde aliases or accessor methods instead of replacing them, so
//!   `.router/registry.json`, `.router/session.json`, the stdout envelope, and the
//!   `ROUTED_*` environment block all keep working for existing harnesses.
//! * New v2-only fields are `#[serde(default)]` (or skipped when empty) so older
//!   readers never see a shape they cannot parse.

use s1s2_core::s2::handoff::PathwayKind;
use s1s2_core::ExecutionPathway;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub use s1s2_core::s2::handoff::SessionLedger as SessionState;

/// One discovered capability. `kind` is load-bearing: `agent` targets a subagent,
/// `skill` contributes instructions. `name` / `capabilities` / `model_tier` are the
/// spec's richer vocabulary and are optional on the wire.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct AgentEntry {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    /// Relative manifest path (spec §4.1 calls this `file_path`).
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Display name; falls back to `id` when the manifest omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Capability keywords used for System 1 density scoring and System 2 tie-breaks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Optional hint for the harness (`opus` / `sonnet` / `haiku`), never enforced here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tier: Option<String>,
}

impl AgentEntry {
    pub fn is_agent(&self) -> bool {
        self.kind == "agent"
    }

    pub fn is_skill(&self) -> bool {
        self.kind == "skill"
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    pub fn file_path(&self) -> &str {
        &self.path
    }

    /// Triggers plus declared capabilities, deduplicated — the keyword surface System 1
    /// matches against.
    pub fn keyword_surface(&self) -> Vec<String> {
        let mut surface: Vec<String> = Vec::new();
        for keyword in self.triggers.iter().chain(self.capabilities.iter()) {
            let lowered = keyword.to_lowercase();
            if !lowered.is_empty() && !surface.contains(&lowered) {
                surface.push(lowered);
            }
        }
        surface
    }
}

/// Cached registry snapshot: capability id -> entry. The `hash` header lives in the
/// cache file itself (`.router/registry.json`), not in this type.
pub type AgentRegistry = BTreeMap<String, AgentEntry>;

/// System 1 input payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptInput {
    pub raw_prompt: String,
    /// Tokens already committed to the context window (harness ledger + overhead).
    #[serde(default)]
    pub context_tokens_used: usize,
    /// Model context window; `0` means "use the configured default".
    #[serde(default)]
    pub context_window_limit: usize,
    #[serde(default)]
    pub session_state: Option<SessionState>,
    /// Harness explicitly asked for the deliberative path.
    #[serde(default)]
    pub force_compile: bool,
}

impl PromptInput {
    pub fn new(raw_prompt: &str) -> Self {
        Self {
            raw_prompt: raw_prompt.to_string(),
            context_tokens_used: 0,
            context_window_limit: 0,
            session_state: None,
            force_compile: false,
        }
    }
}

/// The unified decision payload (spec §4.1). Legacy envelope consumers read the first
/// group of fields; `pathway_metadata` and the handoff artifact are v2 additions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    /// Spec name for the legacy `target_subagent`.
    pub selected_agent: String,
    pub compiled_prompt: String,
    pub saturation_ratio: f32,
    pub circuit_tripped: bool,
    pub handoff_artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathway_metadata: Option<ExecutionPathway>,

    /// Manifest path of `selected_agent`, or `"default"`.
    pub target_subagent_path: String,
    /// Capability ids of matched skills (legacy `required_skills`).
    pub required_skills: Vec<String>,
    /// Manifest paths of matched skills, in the order the legacy dispatcher emitted.
    pub required_skill_paths: Vec<String>,
    pub passthrough: bool,
    /// `CONTINUE_SESSION` | `START_NEW_SESSION` — the legacy action string.
    pub action: String,
    pub session_id: u64,
    /// Legacy `context_handoff_summary`: only populated when the circuit breaker halted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_summary: Option<String>,
}

impl RoutingDecision {
    pub fn pathway_kind(&self) -> PathwayKind {
        if self.circuit_tripped {
            PathwayKind::CircuitBreakerHalt
        } else {
            match &self.pathway_metadata {
                Some(ExecutionPathway::System2Escalated(_)) => PathwayKind::System2Deliberative,
                _ => PathwayKind::System1Fast,
            }
        }
    }
}

/// Domain configuration. `standalone` is the library/embedded profile: no harness state
/// file, no base overhead, no network adjudicator unless asked for.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub repo_root: PathBuf,
    /// Directories scanned for manifests, paired with the kind they declare.
    pub search_dirs: Vec<(String, String)>,
    /// Ledger overhead added to every budget reading. The binary supplies the legacy
    /// `10_000` (system prompt + tool schema); standalone profiling uses `0`.
    pub base_overhead_tokens: usize,
    pub max_window_tokens: usize,
    /// Ratio above which the gate escalates so System 2 can compact (spec: 0.30).
    pub soft_warning_ratio: f32,
    /// Ratio above which the engine halts outright (spec: 0.35).
    pub hard_ceiling_ratio: f32,
    pub session_file: Option<PathBuf>,
    pub registry_cache_file: PathBuf,
    pub handoff_file: Option<PathBuf>,
    pub llm: LlmAdjudicatorConfig,
}

/// Optional System 2 external adjudicator (any OpenAI-compatible endpoint).
#[derive(Debug, Clone, Default)]
pub struct LlmAdjudicatorConfig {
    pub url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_ms: u64,
    pub enabled: bool,
}

/// Directory/kind pairs the legacy router discovered, in scan order.
pub const DEFAULT_SEARCH_DIRS: &[(&str, &str)] = &[
    (".agents", "agent"),
    (".claude/agents", "agent"),
    (".github/agents", "agent"),
    ("skills", "skill"),
    (".claude/skills", "skill"),
    (".github/skills", "skill"),
    (".github/prompts", "skill"),
    (".github/instructions", "skill"),
];

impl RouterConfig {
    /// Library profile: profileable, side-effect-free beyond the registry cache file.
    pub fn standalone(repo_root: PathBuf) -> Self {
        let registry_cache_file = repo_root.join(".router").join("registry.json");
        Self {
            repo_root,
            search_dirs: DEFAULT_SEARCH_DIRS
                .iter()
                .map(|(d, k)| (d.to_string(), k.to_string()))
                .collect(),
            base_overhead_tokens: 0,
            max_window_tokens: s1s2_core::s1::circuit::DEFAULT_CONTEXT_WINDOW,
            soft_warning_ratio: s1s2_core::s1::circuit::DEFAULT_SOFT_WARNING_RATIO,
            hard_ceiling_ratio: s1s2_core::s1::circuit::DEFAULT_HARD_CEILING_RATIO,
            session_file: None,
            registry_cache_file,
            handoff_file: None,
            llm: LlmAdjudicatorConfig::default(),
        }
    }

    /// Harness profile, matching the v0.3.0 defaults exactly.
    pub fn from_env(repo_root: PathBuf) -> Self {
        let base_overhead_tokens = env_usize("ROUTER_BASE_OVERHEAD_TOKENS", 10_000);
        let max_window_tokens = env_usize("ROUTER_MAX_TOKENS", 128_000);
        let router_dir = repo_root.join(".router");
        Self {
            repo_root,
            search_dirs: DEFAULT_SEARCH_DIRS
                .iter()
                .map(|(d, k)| (d.to_string(), k.to_string()))
                .collect(),
            base_overhead_tokens,
            max_window_tokens,
            soft_warning_ratio: s1s2_core::s1::circuit::DEFAULT_SOFT_WARNING_RATIO,
            hard_ceiling_ratio: s1s2_core::s1::circuit::DEFAULT_HARD_CEILING_RATIO,
            session_file: Some(router_dir.join("session.json")),
            registry_cache_file: router_dir.join("registry.json"),
            handoff_file: Some(router_dir.join("HANDOFF.md")),
            llm: LlmAdjudicatorConfig {
                url: std::env::var("ROUTER_LLM_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:11434/v1/chat/completions".to_string()),
                api_key: std::env::var("ROUTER_LLM_KEY").unwrap_or_default(),
                model: std::env::var("ROUTER_LLM_MODEL")
                    .unwrap_or_else(|_| "qwen2.5-coder:7b".to_string()),
                timeout_ms: env_usize("ROUTER_TIMEOUT_MS", 3_000) as u64,
                enabled: true,
            },
        }
    }

    pub fn router_dir(&self) -> PathBuf {
        self.registry_cache_file
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.repo_root.join(".router"))
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
