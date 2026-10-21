//! MCP stdio server (spec §5.3): exposes the engine's three capabilities as tools.
//!
//! The transport is `s1s2_core::harness::mcp`; this module only adapts tool arguments to
//! the same pipeline entry points the CLI uses, so a harness and a hook can never see
//! different routing behaviour from the same repository state.

use crate::envelope;
use crate::pipeline;
use ext_agent_router::AgentRoutingDomain;
use ext_agent_router::types::RouterConfig;
use s1s2_core::harness::mcp::{McpError, McpServer, McpTool};
use s1s2_core::s1::circuit::CircuitBreakerConfig;
use s1s2_core::CognitiveEngine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const SERVER_NAME: &str = "agent-router";

/// Input schema shared by `route_intent` and `check_context_budget`.
fn prompt_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "raw_prompt": { "type": "string", "description": "The user intent to route." },
            "context_tokens_used": {
                "type": "integer",
                "minimum": 0,
                "description": "Override the committed ledger for this call."
            },
            "context_window_limit": {
                "type": "integer",
                "minimum": 0,
                "description": "Override the model context window (0 = configured default)."
            },
            "force_compile": {
                "type": "boolean",
                "description": "Skip System 1 and force the deliberative path."
            }
        },
        "required": ["raw_prompt"]
    })
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, McpError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| McpError::invalid_params(format!("`{key}` must be a non-empty string")))
}

fn arg_usize(args: &Value, key: &str) -> usize {
    args.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn serve(repo_root: &Path) -> Result<(), String> {
    let root = PathBuf::from(repo_root);

    let route_root = root.clone();
    let budget_root = root.clone();
    let refresh_root = root.clone();

    let mut server = McpServer::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
        .tool(McpTool::new(
            "route_intent",
            "Run the full System 1 -> gate -> System 2 pipeline and return the routing envelope.",
            prompt_schema(),
            move |args| {
                let raw_prompt = arg_str(&args, "raw_prompt")?.to_string();
                let override_tokens = arg_usize(&args, "context_tokens_used");
                let observed = if override_tokens > 0 { Some(override_tokens) } else { None };
                let envelope = pipeline::run_turn(
                    &route_root,
                    &raw_prompt,
                    observed,
                    None,
                    arg_bool(&args, "force_compile"),
                )
                .map_err(|e| McpError::internal(format!("pipeline error: {e}")))?;
                envelope::to_value(&envelope).map_err(|e| McpError::internal(e))
            },
        ))
        .tool(McpTool::new(
            "check_context_budget",
            "Report saturation ratio and circuit-breaker state without compiling or committing.",
            prompt_schema(),
            move |args| {
                let raw_prompt = arg_str(&args, "raw_prompt")?;
                pipeline::budget_probe(
                    &budget_root,
                    raw_prompt,
                    arg_usize(&args, "context_tokens_used"),
                    arg_usize(&args, "context_window_limit"),
                )
                .map_err(|e| McpError::internal(format!("budget error: {e}")))
            },
        ))
        .tool(McpTool::new(
            "refresh_registry",
            "Invalidate and rebuild the capability registry snapshot from disk.",
            json!({ "type": "object", "properties": {} }),
            move |_| {
                let config = RouterConfig::from_env(refresh_root.clone());
                let cache_file = config.registry_cache_file.clone();
                let mut engine =
                    CognitiveEngine::<AgentRoutingDomain>::new(config, CircuitBreakerConfig::default());
                engine
                    .rebuild_registry()
                    .map_err(|e| McpError::internal(format!("registry rebuild failed: {e}")))?;
                Ok(json!({
                    "refreshed": true,
                    "cache_file": cache_file.to_string_lossy()
                }))
            },
        ));

    server.serve_stdio().map_err(|e| e.to_string())
}
