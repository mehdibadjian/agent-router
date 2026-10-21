````rust
==> bins/agent-router/src/envelope.rs <==
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

==> bins/agent-router/src/hook.rs <==
//! Native lifecycle-hook adapter (`--hook`), unchanged in wire shape from v0.3.0.
//!
//! VS Code Copilot, Claude Code, and Antigravity all post a JSON request on stdin and read
//! a JSON response on stdout. The only v2 change is where the routing payload comes from:
//! the cognitive engine instead of the old in-binary pipeline.

use crate::envelope::RouterEnvelope;
use crate::pipeline::run_turn;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use s1s2_core::s1::circuit::estimate_tokens;

/// Format the envelope as an imperative context block for Copilot `runSubagent` / Claude Code.
pub fn format_hook_context(envelope: &RouterEnvelope) -> String {
    let mut sections = Vec::new();

    sections.push(format!(
        "<router_meta session_id=\"{}\" saturation=\"{:.3}\" action=\"{}\" subagent=\"{}\" subagent_path=\"{}\" skills=\"{}\" passthrough=\"{}\" />",
        envelope.session_meta.session_id,
        envelope.session_meta.saturation_ratio,
        envelope.session_meta.action,
        envelope.routing.target_subagent,
        envelope.routing.target_subagent_path,
        envelope.routing.required_skills.join(","),
        envelope.routing.passthrough,
    ));

    // Imperative routing directive tailored for VS Code Copilot `runSubagent` + Claude Code `Task`
    if envelope.routing.target_subagent != "default" || !envelope.routing.required_skills.is_empty() {
        let mut directives = Vec::new();
        if envelope.routing.target_subagent != "default" {
            directives.push(format!(
                "- MANDATORY SUBAGENT DELEGATION: You MUST delegate this task to the custom agent `{name}` (defined at `{path}`). In VS Code Copilot, invoke the `runSubagent` (`agent/runSubagent`) tool with `agentName: \"{name}\"` and pass the `<objective>`, `<constraints>`, and `<verification>` blocks below as its `prompt`. If `runSubagent` is unavailable, read `{path}` first and strictly adopt its persona and constraints.",
                name = envelope.routing.target_subagent,
                path = envelope.routing.target_subagent_path
            ));
        }
        if !envelope.routing.required_skills.is_empty() {
            directives.push(format!(
                "- MANDATORY SKILLS: Before writing code, read and follow the skill instructions at: `{}`.",
                envelope.routing.required_skills.join("`, `")
            ));
        }
        sections.push(format!("<routing_directive>\n{}\n</routing_directive>", directives.join("\n")));
    }

    if let Some(handoff) = &envelope.payload.context_handoff_summary {
        if !handoff.is_empty() {
            sections.push(format!("<session_handoff>\n{}\n</session_handoff>", handoff));
        }
    }

    sections.push(envelope.payload.compiled_prompt.clone());
    sections.join("\n\n")
}

/// Extract the latest user prompt from an Antigravity JSONL transcript if present.
fn extract_prompt_from_transcript(transcript_path: &str) -> Option<String> {
    let content = fs::read_to_string(transcript_path).ok()?;
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let is_user_input = entry
            .get("type")
            .and_then(|t| t.as_str())
            .map(|t| t == "USER_INPUT")
            .unwrap_or(false);
        if is_user_input {
            if let Some(text) = entry.get("content").and_then(|c| c.as_str()) {
                if !text.trim().is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// Estimate real session token usage directly from the harness transcript file.
fn estimate_transcript_tokens(req: &Value) -> Option<usize> {
    let path_str = req
        .get("transcript_path")
        .or_else(|| req.get("transcriptPath"))
        .and_then(|p| p.as_str())?;

    let content = fs::read_to_string(path_str).ok()?;
    Some(estimate_tokens(&content))
}

/// Extract the host harness's native session ID (`sessionId` in VS Code, `session_id` in Claude).
fn extract_host_session_id(req: &Value) -> Option<String> {
    req.get("sessionId")
        .or_else(|| req.get("session_id"))
        .or_else(|| req.get("conversationId"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

fn is_agy_hook(input: &Value) -> bool {
    input.get("invocationNum").is_some()
        || input.get("workspacePaths").is_some()
        || input.get("transcriptPath").is_some()
        || input.get("conversationId").is_some()
}

fn hook_event_name(req: &Value) -> String {
    req.get("hookEventName")
        .or_else(|| req.get("hook_event_name"))
        .and_then(|e| e.as_str())
        .unwrap_or("UserPromptSubmit")
        .to_string()
}

/// Reply with `{}` and no routing, which every supported hook treats as "no opinion".
fn fail_open(stdout: &mut impl Write) -> io::Result<()> {
    writeln!(stdout, "{{}}")?;
    stdout.flush()
}

pub fn run_stdio_hook(default_repo_root: &Path) -> io::Result<()> {
    let mut raw_input = String::new();
    io::stdin().read_to_string(&mut raw_input)?;

    let mut stdout = io::stdout();
    let trimmed = raw_input.trim();
    if trimmed.is_empty() {
        return fail_open(&mut stdout);
    }

    let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
        return fail_open(&mut stdout);
    };

    let repo_root: PathBuf = req
        .get("cwd")
        .and_then(|c| c.as_str())
        .or_else(|| req.pointer("/workspacePaths/0").and_then(|p| p.as_str()))
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(|| default_repo_root.to_path_buf());

    let raw_prompt = req
        .get("prompt")
        .and_then(|p| p.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            req.get("transcriptPath")
                .or_else(|| req.get("transcript_path"))
                .and_then(|t| t.as_str())
                .and_then(extract_prompt_from_transcript)
        })
        .unwrap_or_default();

    if raw_prompt.trim().is_empty() {
        return fail_open(&mut stdout);
    }

    let observed_tokens = estimate_transcript_tokens(&req);
    let host_session_id = extract_host_session_id(&req);

    let response = match run_turn(&repo_root, &raw_prompt, observed_tokens, host_session_id, false) {
        Ok(envelope) => {
            let context = format_hook_context(&envelope);
            let blocked = envelope.session_meta.threshold_exceeded;
            let percent = envelope.session_meta.saturation_ratio * 100.0;

            if is_agy_hook(&req) {
                let agy_msg = if blocked {
                    format!(
                        "<circuit_breaker_alert>\nSTOP: Context saturation reached {percent:.1}% (>35% hard ceiling). State handoff saved to `.router/HANDOFF.md`. Start a new conversation.\n</circuit_breaker_alert>\n\n{context}"
                    )
                } else {
                    context
                };
                json!({ "injectSteps": [ { "ephemeralMessage": agy_msg } ] })
            } else if blocked {
                let stop_msg = format!(
                    "⚡ AgentRouter Circuit Breaker: Context reached {percent:.1}% (>35% ceiling). State handoff & compiled prompt saved to .router/HANDOFF.md. Click '+' (New Chat) or run `/clear` and re-submit your prompt."
                );
                // Compatible with BOTH VS Code Copilot (`continue: false`, `stopReason`) and Claude Code (`decision: "block"`, `reason`)
                json!({
                    "continue": false,
                    "stopReason": stop_msg,
                    "systemMessage": stop_msg,
                    "decision": "block",
                    "reason": stop_msg,
                    "hookSpecificOutput": {
                        "hookEventName": hook_event_name(&req),
                        "additionalContext": context
                    }
                })
            } else {
                let skills_label = if envelope.routing.required_skills.is_empty() {
                    "none".to_string()
                } else {
                    envelope.routing.required_skills.join(",")
                };
                let status_banner = format!(
                    "⚡ AgentRouter [{:.1}%/35% ctx | subagent: {} | skills: {}]",
                    envelope.session_meta.saturation_ratio * 100.0,
                    envelope.routing.target_subagent,
                    skills_label
                );
                json!({
                    "continue": true,
                    "systemMessage": status_banner,
                    "hookSpecificOutput": {
                        "hookEventName": hook_event_name(&req),
                        "additionalContext": context
                    }
                })
            }
        }
        Err(e) => {
            eprintln!("[agent-router] Hook pipeline error (failing open): {e}");
            json!({})
        }
    };

    writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::extract_host_session_id;
    use serde_json::json;

    #[test]
    fn host_session_id_accepts_every_harness_spelling() {
        for key in ["sessionId", "session_id", "conversationId"] {
            let req = json!({ key: "host-1" });
            assert_eq!(extract_host_session_id(&req).as_deref(), Some("host-1"), "{key}");
        }
        assert_eq!(extract_host_session_id(&json!({})), None);
    }
}

==> bins/agent-router/src/main.rs <==
//! `agent-router` — thin CLI/MCP entrypoint over the `s1s2` engine (spec §5).
//!
//! The binary owns no cognitive policy: it parses the harness surface, delegates the turn
//! to `CognitiveEngine<AgentRoutingDomain>` through `pipeline`, and presents the result in
//! whichever shape the caller asked for (JSON envelope, lifecycle hook, wrapped child, or
//! MCP tool reply). Drop-in compatibility with v0.3.0 is enforced by
//! `tests/pipeline.rs` / `tests/hooks.rs`.

mod envelope;
mod hook;
mod mcp;
mod pipeline;

use s1s2_core::harness::cli::spawn;
use std::env;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage:
  agent-router \"<raw_prompt>\"                 route a prompt (legacy positional form)
  agent-router --prompt \"<raw_prompt>\"         route a prompt; reads stdin when omitted
  agent-router --hook                          lifecycle-hook mode (Copilot / Claude / Antigravity)
  agent-router --mcp-stdio                     MCP server over newline-delimited JSON-RPC
  agent-router --wrap <CMD> [args...]          route, then run CMD with the ROUTED_* env block
  agent-router --sync-tokens <exact_count>     calibrate the token ledger without routing";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let repo_root = env::current_dir()
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));

    if args.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(1);
    }

    let code = match args[0].as_str() {
        "--hook" => match hook::run_stdio_hook(&repo_root) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[agent-router] Hook error: {e}");
                1
            }
        },
        "--mcp-stdio" => match mcp::serve(&repo_root) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[agent-router] MCP error: {e}");
                1
            }
        },
        "--sync-tokens" => sync_tokens(&repo_root, args.get(1).map(|s| s.as_str())),
        "--prompt" => route_prompt(&repo_root, &args[1..]),
        "--wrap" => wrap(&repo_root, &args[1..]),
        // Legacy behaviour: every other argv is joined into the raw prompt.
        _ => run_and_dispatch(&repo_root, &args.join(" "), false),
    };

    std::process::exit(code);
}

fn sync_tokens(repo_root: &Path, raw: Option<&str>) -> i32 {
    match raw.and_then(|v| v.parse::<usize>().ok()) {
        Some(val) => {
            match pipeline::sync_tokens(repo_root, val) {
                Ok(_) => eprintln!("[agent-router] ledger calibrated to {val} tokens"),
                Err(e) => {
                    eprintln!("[agent-router] failed to persist token ledger: {e}");
                    return 1;
                }
            }
            0
        }
        None => {
            eprintln!("Usage: agent-router --sync-tokens <exact_token_count>");
            1
        }
    }
}

/// `--prompt [text]`: use the given text, else read the prompt from stdin.
fn route_prompt(repo_root: &Path, rest: &[String]) -> i32 {
    let force = rest.iter().any(|a| a == "--force");
    let text: Vec<String> = rest.iter().filter(|a| *a != "--force").cloned().collect();

    if text.is_empty() {
        match read_stdin() {
            Some(prompt) if !prompt.trim().is_empty() => {
                run_and_dispatch(repo_root, prompt.trim(), force)
            }
            _ => {
                eprintln!("[agent-router] --prompt requires text or stdin");
                1
            }
        }
    } else {
        run_and_dispatch(repo_root, &text.join(" "), force)
    }
}

/// `--wrap <CMD> [args...]`: route the remaining arguments and hand the child the prompt.
fn wrap(repo_root: &Path, rest: &[String]) -> i32 {
    let Some((program, args)) = rest.split_first().map(|(p, a)| (p.clone(), a.to_vec())) else {
        eprintln!("Usage: agent-router --wrap <CMD> [args...]");
        return 1;
    };

    if args.is_empty() {
        eprintln!("[agent-router] --wrap needs a prompt after the command");
        return 1;
    }

    let prompt = args.join(" ");
    let envelope = match pipeline::run_turn(repo_root, &prompt, None, None, false) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[agent-router] Pipeline error: {e}");
            return 1;
        }
    };

    let request = envelope::spawn_request(&program, &envelope, &[]);
    match spawn(&request) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("[agent-router] Dispatch error: {e}");
            1
        }
    }
}

/// Route, then dispatch: Mode A (`ROUTER_HARNESS_BIN`) spawns, Mode B prints JSON.
fn run_and_dispatch(repo_root: &Path, raw_prompt: &str, force_compile: bool) -> i32 {
    let envelope = match pipeline::run_turn(repo_root, raw_prompt, None, None, force_compile) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[agent-router] Pipeline error: {e}");
            return 1;
        }
    };

    match env::var("ROUTER_HARNESS_BIN") {
        Ok(program) => {
            let request = envelope::spawn_request(&program, &envelope, &[]);
            match spawn(&request) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("[agent-router] Dispatch error: {e}");
                    1
                }
            }
        }
        Err(_) => match envelope::emit_json(&envelope) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[agent-router] Dispatch error: {e}");
                1
            }
        },
    }
}

fn read_stdin() -> Option<String> {
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer).ok()?;
    if buffer.trim().is_empty() {
        None
    } else {
        Some(buffer)
    }
}

==> bins/agent-router/src/mcp.rs <==
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

==> bins/agent-router/src/pipeline.rs <==
//! The thin pipeline runner (spec §5.1): ledger state in, cognitive envelope out.
//!
//! All cognitive decisions come from `CognitiveEngine<AgentRoutingDomain>`. This module
//! owns only harness state — the `.router/session.json` ledger, host-session resets, and
//! the `.router/HANDOFF.md` file that carries continuity across a blocked turn.

use crate::envelope::{build_envelope, RouterEnvelope};
use ext_agent_router::types::{PromptInput, RoutingDecision, RouterConfig};
use ext_agent_router::AgentRoutingDomain;
use s1s2_core::s1::circuit::{estimate_tokens, CircuitBreakerConfig};
use s1s2_core::s2::handoff::{commit_turn, load_ledger, save_ledger, SessionLedger};
use s1s2_core::CognitiveEngine;
use std::fs;
use std::path::Path;

/// Run one cognitive turn and return the legacy-shaped envelope, with the ledger committed.
pub fn run_turn(
    repo_root: &Path,
    raw_prompt: &str,
    observed_transcript_tokens: Option<usize>,
    host_session_id: Option<String>,
    force_compile: bool,
) -> Result<RouterEnvelope, String> {
    let config = RouterConfig::from_env(repo_root.to_path_buf());
    fs::create_dir_all(config.router_dir()).map_err(|e| e.to_string())?;

    let session_file = config
        .session_file
        .clone()
        .unwrap_or_else(|| config.router_dir().join("session.json"));
    let mut ledger = load_ledger(&session_file);

    // A new host session (`+` / `/clear`) resets the counter, but a pending handoff is
    // deliberately left on disk so the first real turn of the new session can consume it.
    if let Some(new_host_id) = host_session_id.clone() {
        if ledger.host_session_id.as_ref() != Some(&new_host_id) {
            if ledger.host_session_id.is_some() || config.handoff_file.as_ref().is_some_and(|p| p.exists())
            {
                ledger.accumulated_tokens = 0;
            }
            ledger.host_session_id = Some(new_host_id);
        }
    }

    // A live transcript measurement is authoritative when it exceeds our own estimate.
    if let Some(real_tokens) = observed_transcript_tokens {
        if real_tokens > ledger.accumulated_tokens {
            ledger.accumulated_tokens = real_tokens;
        }
    }

    let input = PromptInput {
        raw_prompt: raw_prompt.to_string(),
        context_tokens_used: ledger.accumulated_tokens,
        context_window_limit: 0,
        session_state: Some(ledger.clone()),
        force_compile,
    };

    let mut engine = CognitiveEngine::<AgentRoutingDomain>::new(
        config.clone(),
        CircuitBreakerConfig::default(),
    );
    let (ratio, _, _, _) = engine.budget_check(&input);
    let prompt_tokens = estimate_tokens(raw_prompt);

    let envelope = engine.process(&input)?;
    let mut decision: RoutingDecision = envelope.decision;
    let pathway = envelope.pathway;
    let (latency_micros, cache_hit) = (envelope.latency_micros, envelope.cache_hit);
    let breached = decision.circuit_tripped;

    // Persist or consume `.router/HANDOFF.md` across the blocked -> fresh session boundary.
    let mut handoff_summary = decision.handoff_summary.clone();
    if let Some(handoff_file) = config.handoff_file.as_ref() {
        if breached {
            if !handoff_file.exists() {
                if let Some(summary) = &handoff_summary {
                    let _ = fs::write(handoff_file, summary);
                }
            }
        } else if handoff_file.exists() && !decision.passthrough {
            let pending = fs::read_to_string(handoff_file).unwrap_or_default();
            let trimmed = pending.trim();
            if !trimmed.is_empty() && handoff_summary.is_none() {
                handoff_summary = Some(trimmed.to_string());
            }
            let _ = fs::remove_file(handoff_file);
        }
    }

    // The envelope and the ledger both report the handoff that actually took effect,
    // including one restored from a pending `.router/HANDOFF.md`.
    decision.handoff_summary = handoff_summary;

    let prior_tokens = ledger.accumulated_tokens;
    commit_turn(
        &mut ledger,
        breached,
        pathway.kind(),
        raw_prompt,
        &decision.compiled_prompt,
        decision.handoff_summary.clone(),
        estimate_tokens,
    );
    // In hook mode a blocked turn stays blocked until the host session id actually
    // changes, so pin the counter rather than adopting the rolled-forward value.
    if breached && host_session_id.is_some() {
        ledger.accumulated_tokens = prior_tokens;
    }
    save_ledger(&session_file, &ledger).map_err(|e| e.to_string())?;

    eprintln!(
        "[agent-router] prompt_tokens={prompt_tokens} projected_saturation={:.3} reset={breached}",
        ratio
    );

    Ok(build_envelope(
        &ledger,
        &decision,
        ratio,
        &pathway,
        latency_micros,
        cache_hit,
    ))
}

/// Calibrate the ledger directly, without routing a turn.
pub fn sync_tokens(repo_root: &Path, tokens: usize) -> Result<SessionLedger, String> {
    let config = RouterConfig::from_env(repo_root.to_path_buf());
    fs::create_dir_all(config.router_dir()).map_err(|e| e.to_string())?;
    let session_file = config
        .session_file
        .clone()
        .unwrap_or_else(|| config.router_dir().join("session.json"));
    let mut ledger = load_ledger(&session_file);
    ledger.accumulated_tokens = tokens;
    save_ledger(&session_file, &ledger).map_err(|e| e.to_string())?;
    Ok(ledger)
}

/// Read the current ledger for harness probes that must not commit a turn.
pub fn read_ledger(repo_root: &Path) -> SessionLedger {
    let config = RouterConfig::from_env(repo_root.to_path_buf());
    config
        .session_file
        .as_ref()
        .map(|path| load_ledger(path))
        .unwrap_or_default()
}

/// The budget probe exposed as MCP `check_context_budget`: no ledger commit, no compile.
pub fn budget_probe(
    repo_root: &Path,
    raw_prompt: &str,
    context_tokens_used: usize,
    context_window_limit: usize,
) -> Result<serde_json::Value, String> {
    let config = RouterConfig::from_env(repo_root.to_path_buf());
    let ledger = read_ledger(repo_root);
    let input = PromptInput {
        raw_prompt: raw_prompt.to_string(),
        context_tokens_used: if context_tokens_used > 0 { context_tokens_used } else { ledger.accumulated_tokens },
        context_window_limit,
        session_state: Some(ledger.clone()),
        force_compile: false,
    };

    let engine = CognitiveEngine::<AgentRoutingDomain>::new(
        config.clone(),
        CircuitBreakerConfig::default(),
    );
    let (ratio, used, limit, trip) = engine.budget_check(&input);
    let ratio = ((ratio as f64) * 1000.0).round() / 1000.0;
    Ok(serde_json::json!({
        "saturation_ratio": ratio,
        "used_tokens": used,
        "limit_tokens": limit,
        "circuit_tripped": trip.is_some(),
        "hard_ceiling_ratio": round3(config.hard_ceiling_ratio),
        "soft_warning_ratio": round3(config.soft_warning_ratio),
        "session_id": ledger.session_id,
        "accumulated_tokens": ledger.accumulated_tokens,
    }))
}

/// Serialize a configured ratio as the exact 3-decimal value it was meant to be; an `f32`
/// `0.35` prints as `0.3499999940395355`, which is technically right and useless to a reader.
fn round3(ratio: f32) -> f64 {
    ((ratio as f64) * 1000.0).round() / 1000.0
}

==> bins/agent-router/tests/common/mod.rs <==
//! Shared helpers for the integration test targets. Each target uses a subset of
//! these helpers, so dead-code warnings here are expected and silenced.
#![allow(dead_code)]

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Environment keys that must be neutralised so runs stay deterministic.
const ISOLATED_VARS: &[&str] = &[
    "ROUTER_LLM_URL",
    "ROUTER_LLM_KEY",
    "ROUTER_LLM_MODEL",
    "ROUTER_TIMEOUT_MS",
    "ROUTER_BASE_OVERHEAD_TOKENS",
    "ROUTER_MAX_TOKENS",
    "ROUTER_HARNESS_BIN",
];

/// Run the built binary in `cwd` with `envs`, optionally feeding stdin.
/// Returns (stdout, stderr, exit_code).
pub fn run(cwd: &str, args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>) -> (String, String, i32) {
    let bin = env!("CARGO_BIN_EXE_agent-router");
    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd).args(args);
    for k in ISOLATED_VARS {
        cmd.env_remove(k);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn agent-router");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait for child");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Fresh temp repo seeded with one agent and one skill manifest.
pub fn fixture_repo(tag: &str) -> String {
    let root = std::env::temp_dir().join(format!("agent-router-it-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".agents")).expect("mkdir agents");
    std::fs::create_dir_all(root.join("skills/tdd")).expect("mkdir skills");
    std::fs::write(
        root.join(".agents/rust-architect.md"),
        "---\nname: rust-architect\ndescription: Systems architect\ntriggers: [rust, borrow, lifetime]\n---\nBody\n",
    )
    .expect("write agent");
    std::fs::write(
        root.join("skills/tdd/SKILL.md"),
        "---\nname: tdd-workflow\ndescription: Red-Green-Refactor\ntriggers: [test, tdd]\n---\nBody\n",
    )
    .expect("write skill");
    root.to_string_lossy().into_owned()
}

/// Minimal OpenAI-compatible mock built purely on std::net (no extra crates).
pub struct MockLlm {
    pub url: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<std::sync::Mutex<bool>>,
}

impl MockLlm {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        *self.stop.lock().unwrap() = true;
    }
}

/// Spawn a mock that answers every POST with `content` as the assistant message.
pub fn spawn_mock_llm(content: String) -> MockLlm {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock llm");
    let port = listener.local_addr().expect("mock addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(std::sync::Mutex::new(false));

    let (hits_t, stop_t) = (hits.clone(), stop.clone());
    std::thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        loop {
            if *stop_t.lock().unwrap() {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Read until the head terminator; the body is irrelevant to the reply.
                    let mut buf = Vec::new();
                    let mut scratch = [0u8; 2048];
                    loop {
                        let n = stream.read(&mut scratch).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&scratch[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        if buf.len() > 262_144 {
                            break;
                        }
                    }
                    hits_t.fetch_add(1, Ordering::SeqCst);
                    let payload = serde_json::json!({
                        "choices": [{ "message": { "role": "assistant", "content": content } }]
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    MockLlm {
        url: format!("http://127.0.0.1:{}/v1/chat/completions", port),
        hits,
        stop,
    }
}

==> bins/agent-router/tests/harness.rs <==
//! Spec §5 harness tests for the thin binary: `--prompt`/stdin, `--mcp-stdio`,
//! `--wrap <CMD>`, and the legacy `--hook`/`--sync-tokens`/positional surface.
//!
//! These are compatibility tests: the v2 binary must keep the v0.3.0 drop-in behaviour
//! (JSON envelopes, `ROUTED_*` env block, exit-code propagation) while adding the v2
//! flags on top of it.

mod common;

use common::fixture_repo;
use common::run;
use serde_json::Value;

const OFFLINE: &[(&str, &str)] = &[
    ("ROUTER_LLM_URL", "http://127.0.0.1:1/v1/chat/completions"),
    ("ROUTER_TIMEOUT_MS", "150"),
];

fn read_json(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read state file"))
        .expect("parse state")
}

/// One newline-delimited JSON-RPC frame per line.
fn rpc(id: u64, method: &str, params: Value) -> String {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("serialize frame")
}

fn lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad frame {l}: {e}")))
        .collect()
}

#[test]
fn explicit_prompt_flag_emits_the_legacy_envelope() {
    let repo = fixture_repo("cli-prompt");
    let (stdout, _stderr, code) = run(
        &repo,
        &["--prompt", "fix rust lifetime in parser and add test"],
        OFFLINE,
        None,
    );
    assert_eq!(code, 0);
    let envelope: Value = serde_json::from_str(&stdout).expect("pure JSON envelope");
    assert_eq!(envelope["routing"]["target_subagent"], "rust-architect");
    assert_eq!(envelope["session_meta"]["action"], "CONTINUE_SESSION");
    assert!(envelope["payload"]["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("<objective>"));

    // The ledger is committed exactly like the legacy binary did.
    let state = read_json(&format!("{}/.router/session.json", repo));
    assert_eq!(state["session_id"].as_u64().unwrap(), 1);
    assert!(state["accumulated_tokens"].as_u64().unwrap() > 0);
    assert!(state["rolling_summary"]
        .as_str()
        .unwrap()
        .contains("fix rust lifetime"));
}

#[test]
fn prompt_flag_without_value_reads_stdin() {
    let repo = fixture_repo("cli-stdin");
    let (stdout, _stderr, code) = run(&repo, &["--prompt"], OFFLINE, Some("adopt tdd workflow\n"));
    assert_eq!(code, 0, "stdin prompt must route");
    let envelope: Value = serde_json::from_str(&stdout).expect("envelope");
    assert_eq!(envelope["routing"]["passthrough"], false);
    assert!(envelope["payload"]["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("adopt tdd workflow"));
}

#[test]
fn wrap_dispatches_to_the_child_with_the_routed_env_block() {
    let repo = fixture_repo("cli-wrap");
    let harness = format!("{}/fake_harness.sh", repo);
    std::fs::write(
        &harness,
        "#!/bin/sh\necho \"$ROUTED_SUBAGENT|$ROUTED_SESSION_ACTION|$ROUTED_SKILLS|${1:-}\" > marker.txt\nexit 7\n",
    )
    .expect("write harness");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let (stdout, _stderr, code) = run(
        &repo,
        &["--wrap", harness.as_str(), "fix", "rust", "lifetime", "bug"],
        OFFLINE,
        None,
    );
    assert_eq!(code, 7, "wrap must propagate the child exit code");
    assert!(stdout.is_empty(), "wrap mode must not print the envelope: {stdout}");

    let marker = std::fs::read_to_string(format!("{}/marker.txt", repo)).expect("marker");
    let mut parts = marker.trim().split('|');
    assert_eq!(parts.next(), Some("rust-architect"));
    assert_eq!(parts.next(), Some("CONTINUE_SESSION"));
    let prompt = parts.nth(1).unwrap_or("");
    assert!(prompt.contains("<objective>"), "child got the compiled prompt");
    assert!(prompt.contains("fix rust lifetime bug"), "child prompt: {prompt}");
}

#[test]
fn mcp_stdio_serves_the_three_router_tools() {
    let repo = fixture_repo("mcp-basic");
    let frames = vec![
        rpc(1, "initialize", serde_json::json!({"protocolVersion": "2024-11-05"})),
        rpc(2, "tools/list", serde_json::json!({})),
    ];
    let (stdout, stderr, code) = run(
        &repo,
        &["--mcp-stdio"],
        OFFLINE,
        Some(&format!("{}\n", frames.join("\n"))),
    );
    assert_eq!(code, 0, "mcp server must exit cleanly: {stderr}");

    let replies = lines(&stdout);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "agent-router");

    let tools = replies[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"route_intent")
            && names.contains(&"check_context_budget")
            && names.contains(&"refresh_registry"),
        "expected the three router tools, got {names:?}"
    );
    assert_eq!(tools[0]["inputSchema"]["type"], "object");
}

#[test]
fn mcp_route_intent_returns_the_envelope_and_persists_state() {
    let repo = fixture_repo("mcp-route");
    let frames = vec![
        rpc(
            1,
            "tools/call",
            serde_json::json!({
                "name": "route_intent",
                "arguments": {"raw_prompt": "fix rust lifetime in parser", "force_compile": false}
            }),
        ),
        // A notification must never be answered.
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}".to_string(),
        rpc(
            2,
            "tools/call",
            serde_json::json!({
                "name": "check_context_budget",
                "arguments": {"raw_prompt": "implement a trie", "context_tokens_used": 75000}
            }),
        ),
    ];
    let (stdout, _stderr, code) = run(
        &repo,
        &["--mcp-stdio"],
        OFFLINE,
        Some(&format!("{}\n", frames.join("\n"))),
    );
    assert_eq!(code, 0);

    let replies = lines(&stdout);
    assert_eq!(replies.len(), 2, "notifications get no reply");

    let routed: Value = serde_json::from_str(
        replies[0]["result"]["content"][0]["text"]
            .as_str()
            .expect("route text"),
    )
    .expect("route envelope json");
    assert_eq!(routed["routing"]["target_subagent"], "rust-architect");
    assert_eq!(routed["session_meta"]["action"], "CONTINUE_SESSION");
    assert_eq!(replies[0]["result"]["isError"], false);

    let budget: Value = serde_json::from_str(
        replies[1]["result"]["content"][0]["text"]
            .as_str()
            .expect("budget text"),
    )
    .expect("budget json");
    assert!(budget["circuit_tripped"].as_bool().unwrap(), "{budget}");
    assert_eq!(budget["hard_ceiling_ratio"], serde_json::json!(0.35));
    assert!(budget["saturation_ratio"].as_f64().unwrap() > 0.35);

    // route_intent committed a turn to the same ledger the CLI uses.
    let state = read_json(&format!("{}/.router/session.json", repo));
    assert!(state["accumulated_tokens"].as_u64().unwrap() > 0);
}

#[test]
fn mcp_errors_and_refresh_registry_are_well_formed() {
    let repo = fixture_repo("mcp-errors");
    // Seed the cache so refresh has something to drop.
    run(&repo, &["probe"], OFFLINE, None);
    let cache = format!("{}/.router/registry.json", repo);
    assert!(std::path::Path::new(&cache).exists());
    std::fs::write(&cache, "not json at all").expect("clobber cache");

    let frames = vec![
        rpc(1, "tools/call", serde_json::json!({"name": "nope", "arguments": {}})),
        rpc(2, "tools/call", serde_json::json!({"name": "route_intent", "arguments": {}})),
        rpc(3, "tools/call", serde_json::json!({"name": "refresh_registry", "arguments": {}})),
        rpc(4, "bogus/method", serde_json::json!({})),
        "{ broken json".to_string(),
    ];
    let (stdout, _stderr, code) = run(
        &repo,
        &["--mcp-stdio"],
        OFFLINE,
        Some(&format!("{}\n", frames.join("\n"))),
    );
    assert_eq!(code, 0);

    let replies = lines(&stdout);
    assert_eq!(replies.len(), 5);
    assert_eq!(replies[0]["error"]["code"], -32602, "unknown tool");
    // A missing required argument is the tool's failure, not the protocol's (MCP semantics).
    assert_eq!(replies[1]["result"]["isError"], true, "missing raw_prompt");
    assert!(
        replies[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("raw_prompt"),
        "{}",
        replies[1]
    );
    assert_eq!(replies[2]["result"]["isError"], false, "refresh ok");
    assert_eq!(replies[3]["error"]["code"], -32601, "unknown method");
    assert_eq!(replies[4]["error"]["code"], -32700, "parse error");

    // refresh_registry rebuilt the snapshot from the manifests.
    let entries = read_json(&cache)["entries"]
        .as_object()
        .cloned()
        .expect("entries map");
    assert!(entries.contains_key("rust-architect"), "{entries:?}");
    assert!(entries.contains_key("tdd-workflow"), "{entries:?}");
}

#[test]
fn unknown_flag_still_routes_as_a_legacy_positional_prompt() {
    let repo = fixture_repo("cli-positional");
    let (stdout, _stderr, code) = run(&repo, &["fix", "rust", "lifetime"], OFFLINE, None);
    assert_eq!(code, 0);
    let envelope: Value = serde_json::from_str(&stdout).expect("envelope");
    assert_eq!(envelope["routing"]["target_subagent"], "rust-architect");
    assert!(envelope["payload"]["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("fix rust lifetime"));
}

#[test]
fn no_arguments_prints_usage_and_exits_nonzero() {
    let repo = fixture_repo("cli-usage");
    let (stdout, stderr, code) = run(&repo, &[], OFFLINE, None);
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    for flag in ["--hook", "--sync-tokens", "--prompt", "--mcp-stdio", "--wrap"] {
        assert!(stderr.contains(flag), "usage must document {flag}: {stderr}");
    }
}

==> bins/agent-router/tests/hooks.rs <==
//! Integration tests for native lifecycle hook mode (`agent-router --hook`):
//! - VS Code Copilot & Claude Code (`UserPromptSubmit` -> `hookSpecificOutput.additionalContext`)
//! - Antigravity / AGY (`PreInvocation` -> `injectSteps[].ephemeralMessage`)

mod common;

use common::fixture_repo;
use common::run;
use serde_json::{json, Value};

const OFFLINE: &[(&str, &str)] = &[
    ("ROUTER_LLM_URL", "http://127.0.0.1:1/v1/chat/completions"),
    ("ROUTER_TIMEOUT_MS", "150"),
];

#[test]
fn copilot_and_claude_user_prompt_submit_hook_emits_additional_context() {
    let repo = fixture_repo("hook-copilot-claude");
    let stdin_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "cwd": repo,
        "prompt": "fix rust lifetime in parser and add test"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON output");
    assert_eq!(resp["continue"], true);
    assert!(resp["systemMessage"]
        .as_str()
        .unwrap_or_default()
        .contains("⚡ AgentRouter ["));
    assert_eq!(
        resp["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("subagent=\"rust-architect\""));
    assert!(ctx.contains("skills=\"skills/tdd/SKILL.md\""));
    assert!(ctx.contains("<routing_directive>"));
    assert!(ctx.contains("MANDATORY SUBAGENT DELEGATION: You MUST delegate this task to the custom agent `rust-architect`"));
    assert!(ctx.contains("runSubagent"));
    assert!(ctx.contains("MANDATORY SKILLS: Before writing code, read and follow the skill instructions at: `skills/tdd/SKILL.md`"));
    assert!(ctx.contains("<objective>"));
}

#[test]
fn agy_pre_invocation_hook_reads_transcript_and_emits_inject_steps() {
    let repo = fixture_repo("hook-agy");
    let transcript_path = format!("{}/transcript.jsonl", repo);
    let transcript_lines = [
        json!({"step_index": 0, "type": "USER_INPUT", "content": "initial prompt"}).to_string(),
        json!({"step_index": 1, "type": "PLANNER_RESPONSE", "content": "done"}).to_string(),
        json!({"step_index": 2, "type": "USER_INPUT", "content": "fix rust lifetime in parser"}).to_string(),
        // Partially flushed / malformed trailing line must be skipped without aborting reverse scan
        "{\"truncated_json_line\": ".to_string(),
    ]
    .join("\n");
    std::fs::write(&transcript_path, transcript_lines).expect("write transcript");

    let stdin_payload = json!({
        "conversationId": "conv-123",
        "workspacePaths": [repo],
        "transcriptPath": transcript_path,
        "invocationNum": 2
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid AGY hook JSON");
    let msg = resp["injectSteps"][0]["ephemeralMessage"]
        .as_str()
        .expect("ephemeralMessage string");
    assert!(msg.contains("subagent=\"rust-architect\""));
    assert!(msg.contains("<routing_directive>"));
    assert!(msg.contains("<objective>"));
}

#[test]
fn hook_blocks_and_saves_handoff_when_ceiling_exceeded_then_injects_on_new_session() {
    let repo = fixture_repo("hook-reset");

    // Turn 1 (normal turn): builds rolling_summary breadcrumb before hitting ceiling
    let turn1_payload = json!({
        "hookEventName": "UserPromptSubmit",
        "sessionId": "vscode-session-1",
        "cwd": repo,
        "prompt": "design ast node structs"
    })
    .to_string();
    let (_s1, _e1, c1) = run(&repo, &["--hook"], OFFLINE, Some(&turn1_payload));
    assert_eq!(c1, 0);

    let (_s, _e, code) = run(&repo, &["--sync-tokens", "36000"], OFFLINE, None);
    assert_eq!(code, 0);

    let stdin_payload = json!({
        "hookEventName": "UserPromptSubmit",
        "sessionId": "vscode-session-1",
        "cwd": repo,
        "prompt": "implement rust parser"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON");
    assert_eq!(resp["continue"], false);
    assert_eq!(resp["decision"], "block");
    assert!(resp["stopReason"]
        .as_str()
        .unwrap_or_default()
        .contains(">35% ceiling"));
    assert!(resp["reason"]
        .as_str()
        .unwrap_or_default()
        .contains(">35% ceiling"));
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("action=\"START_NEW_SESSION\""));
    assert!(ctx.contains("<session_handoff>"));
    assert!(
        ctx.contains("design ast node structs"),
        "handoff must include prior turn breadcrumb from rolling_summary"
    );

    // Verify `.router/HANDOFF.md` was persisted for the fresh session
    let handoff_path = format!("{}/.router/HANDOFF.md", repo);
    assert!(
        std::path::Path::new(&handoff_path).exists(),
        "HANDOFF.md must be written when >35% reset triggers"
    );

    // If user ignores the block and sends another prompt in the SAME `sessionId`, it must keep blocking!
    let (stdout_retry, _stderr_retry, code_retry) =
        run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code_retry, 0);
    let resp_retry: Value = serde_json::from_str(stdout_retry.trim()).expect("valid hook JSON");
    assert_eq!(
        resp_retry["continue"], false,
        "retrying in the same bloated sessionId must remain blocked"
    );
    assert!(
        std::path::Path::new(&handoff_path).exists(),
        "HANDOFF.md must remain intact until a new sessionId starts"
    );

    // If the very first turn in the fresh session is a trivial passthrough (`git status`),
    // HANDOFF.md must be held until the first non-passthrough coding prompt!
    let passthrough_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "sessionId": "fresh-session-2",
        "cwd": repo,
        "prompt": "git status"
    })
    .to_string();
    let (_sp, _ep, cp) = run(&repo, &["--hook"], OFFLINE, Some(&passthrough_payload));
    assert_eq!(cp, 0);
    assert!(
        std::path::Path::new(&handoff_path).exists(),
        "HANDOFF.md must not be consumed on a trivial passthrough turn"
    );

    // First real coding turn on the fresh host session consumes HANDOFF.md
    let fresh_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "sessionId": "fresh-session-2",
        "cwd": repo,
        "prompt": "implement rust parser"
    })
    .to_string();

    let (stdout2, _stderr2, code2) = run(&repo, &["--hook"], OFFLINE, Some(&fresh_payload));
    assert_eq!(code2, 0);
    let resp2: Value = serde_json::from_str(stdout2.trim()).expect("valid hook JSON");
    assert_eq!(resp2["continue"], true);
    assert!(resp2.get("decision").is_none(), "fresh session must not block");
    let ctx2 = resp2["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx2.contains("action=\"CONTINUE_SESSION\""));
    assert!(
        ctx2.contains("<session_handoff>"),
        "fresh session first turn must inject saved handoff"
    );
    assert!(
        ctx2.contains("design ast node structs"),
        "fresh session handoff must preserve original Turn 1 breadcrumb"
    );
    assert!(
        !std::path::Path::new(&handoff_path).exists(),
        "HANDOFF.md must be deleted after consumption"
    );
}

#[test]
fn transcript_path_token_measurement_triggers_ceiling_without_manual_sync() {
    let repo = fixture_repo("hook-transcript-tokens");
    let transcript_path = format!("{}/bloated.jsonl", repo);
    // ~140,000 chars -> ~38,889 tokens + 10,000 base overhead > 44,800 (35% of 128k)
    let bloated_line = json!({
        "type": "ASSISTANT_RESPONSE",
        "content": "x".repeat(140_000)
    })
    .to_string();
    std::fs::write(&transcript_path, bloated_line).expect("write bloated transcript");

    let stdin_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "sess-bloated",
        "transcript_path": transcript_path,
        "cwd": repo,
        "prompt": "fix rust lifetime in parser"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON");
    assert_eq!(resp["continue"], false);
    assert_eq!(resp["decision"], "block");
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("action=\"START_NEW_SESSION\""));
}

#[test]
fn github_agents_and_skills_directories_are_discovered() {
    let repo = fixture_repo("hook-github-dirs");
    std::fs::create_dir_all(format!("{}/.github/agents", repo)).unwrap();
    std::fs::create_dir_all(format!("{}/.github/skills/gh-actions", repo)).unwrap();
    std::fs::create_dir_all(format!("{}/.github/prompts", repo)).unwrap();
    std::fs::create_dir_all(format!("{}/.github/instructions", repo)).unwrap();

    // Omit `name:` in frontmatter to verify `.agent.md`, `.prompt.md`, and `.instructions.md` double-extension stripping
    std::fs::write(
        format!("{}/.github/agents/ci-bot.agent.md", repo),
        "---\ndescription: GitHub Actions specialist\ntriggers: [workflow, pipeline]\n---\n",
    )
    .unwrap();
    std::fs::write(
        format!("{}/.github/skills/gh-actions/SKILL.md", repo),
        "---\nname: gh-actions\ndescription: Actions YAML rules\ntriggers: [workflow]\n---\n",
    )
    .unwrap();
    std::fs::write(
        format!("{}/.github/prompts/deploy-check.prompt.md", repo),
        "---\ndescription: Deployment checklist prompt\ntriggers: [workflow]\n---\n",
    )
    .unwrap();
    std::fs::write(
        format!("{}/.github/instructions/ci-security.instructions.md", repo),
        "---\ndescription: CI security instructions\ntriggers: [pipeline]\n---\n",
    )
    .unwrap();

    let stdin_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "cwd": repo,
        "prompt": "fix workflow pipeline"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON");
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("subagent=\"ci-bot\""));
    assert!(ctx.contains(".github/skills/gh-actions/SKILL.md"));
    assert!(ctx.contains(".github/prompts/deploy-check.prompt.md"));
    assert!(ctx.contains(".github/instructions/ci-security.instructions.md"));
}

#[test]
fn malformed_or_empty_hook_input_fails_open_cleanly() {
    let repo = fixture_repo("hook-malformed");
    for payload in ["", "   \n  ", "not json at all", "{\"no_prompt\": true}"] {
        let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(payload));
        assert_eq!(code, 0, "hook must exit 0 on malformed/empty input");
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("must emit valid JSON");
        assert_eq!(parsed, json!({}));
    }
}

==> bins/agent-router/tests/pipeline.rs <==
//! End-to-end pipeline tests: Step 0 circuit breaker, Step 1 compiler fail-open
//! and LLM paths, Step 2 cache, Step 3 dispatch modes.

mod common;

use common::fixture_repo;
use common::run;
use common::spawn_mock_llm;
use serde_json::Value;

/// Point the router at an unreachable port with a tiny timeout to force fail-open.
const OFFLINE: &[(&str, &str)] = &[
    ("ROUTER_LLM_URL", "http://127.0.0.1:1/v1/chat/completions"),
    ("ROUTER_TIMEOUT_MS", "150"),
];

fn read_json(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read state file")).expect("parse state")
}

#[test]
fn stdout_hook_mode_emits_only_valid_json() {
    let repo = fixture_repo("pipe-hook");
    let (stdout, stderr, code) = run(&repo, &["fix rust lifetime in parser and add test"], OFFLINE, None);
    assert_eq!(code, 0);
    // Stdout must parse as a single JSON envelope (no log lines mixed in).
    let envelope: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be pure JSON: {e}\n{stdout}"));
    // Diagnostics belong on stderr only.
    assert!(
        stderr.contains("agent-router"),
        "fail-open warning should be logged to stderr"
    );
    assert_eq!(envelope["routing"]["target_subagent"], "rust-architect");
    assert_eq!(
        envelope["routing"]["required_skills"],
        serde_json::json!(["skills/tdd/SKILL.md"])
    );
    assert_eq!(envelope["session_meta"]["action"], "CONTINUE_SESSION");
    assert!(envelope["payload"]["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("<objective>"));
}

#[test]
fn fail_open_never_crashes_on_unreachable_llm() {
    let repo = fixture_repo("pipe-failopen");
    let (_stdout, _stderr, code) = run(&repo, &["implement a trie"], OFFLINE, None);
    // 134 would indicate SIGABRT (panic under panic=abort).
    assert_eq!(code, 0, "fail-open must return success, not abort");
}

#[test]
fn llm_happy_path_resolves_fence_and_sanitizes_hallucinations() {
    let repo = fixture_repo("pipe-llm");
    // Model invents a fake agent and a fake skill, and mislabels an agent as a skill.
    let inner = serde_json::json!({
        "passthrough": false,
        "target_subagent": "quantum-unicorn-expert",
        "required_skills": ["tdd-workflow", "made-up-skill", "rust-architect"],
        "compiled_prompt": "<objective>mocked</objective>",
        "context_handoff_summary": null
    });
    // Realistic model behaviour: wrap the whole reply in a markdown fence.
    let content = format!("```json\n{}\n```", inner);
    let mock = spawn_mock_llm(content);

    let envs = [
        ("ROUTER_LLM_URL", mock.url.as_str()),
        ("ROUTER_TIMEOUT_MS", "2000"),
    ];
    let (stdout, stderr, code) = run(&repo, &["anything"], &envs, None);
    mock.stop();

    assert_eq!(code, 0);
    let envelope: Value = serde_json::from_str(&stdout).expect("envelope");
    // Fabricated agent must be rejected and downgraded to default.
    assert_eq!(envelope["routing"]["target_subagent"], "default");
    // Only the genuinely registered skill survives; agent-as-skill and fake skill dropped.
    assert_eq!(
        envelope["routing"]["required_skills"],
        serde_json::json!(["skills/tdd/SKILL.md"])
    );
    // A fence-wrapped valid reply means we never hit fail-open, so no warning on stderr.
    assert!(
        !stderr.contains("Failing open"),
        "should not fall open when LLM responded: {stderr}"
    );
    assert!(mock.hits() >= 1);
}

#[test]
fn three_five_percent_ceiling_rolls_session_id_forward() {
    let repo = fixture_repo("pipe-reset");
    // Sync the ledger near the threshold (36000 + 10000 overhead = 46000 / 128000 = 35.9%).
    let (_s, _e, code) = run(&repo, &["--sync-tokens", "36000"], OFFLINE, None);
    assert_eq!(code, 0);
    let state = read_json(&format!("{}/.router/session.json", repo));
    assert_eq!(state["session_id"].as_u64().unwrap(), 1);
    assert_eq!(state["accumulated_tokens"].as_u64().unwrap(), 36000);

    // Next prompt pushes cumulative usage above 35% and must trigger a session roll.
    let (stdout, _, code) = run(&repo, &["implement next module"], OFFLINE, None);
    assert_eq!(code, 0);
    let env: Value = serde_json::from_str(&stdout).expect("envelope");
    assert_eq!(env["session_meta"]["threshold_exceeded"], true);
    assert_eq!(env["session_meta"]["action"], "START_NEW_SESSION");
    // Ledger is committed before the envelope is built, so both report the rolled id.
    assert_eq!(env["session_meta"]["session_id"].as_u64().unwrap(), 2);
    let after = read_json(&format!("{}/.router/session.json", repo));
    assert_eq!(after["session_id"].as_u64().unwrap(), 2);
    // A handoff summary must be seeded into the new session's rolling context.
    assert!(!after["rolling_summary"].as_str().unwrap().is_empty());
    // Reset clears the accumulated ledger down to handoff + compiled prompt tokens.
    assert!(after["accumulated_tokens"].as_u64().unwrap() < 36000);
}

#[test]
fn trivial_passthrough_skips_llm_entirely() {
    let repo = fixture_repo("pipe-passthru");
    // Mock LLM would increment hits if called; we point at it and expect zero calls.
    let mock = spawn_mock_llm("{}".to_string());
    let envs = [
        ("ROUTER_LLM_URL", mock.url.as_str()),
        ("ROUTER_TIMEOUT_MS", "2000"),
    ];
    let (stdout, _stderr, code) = run(&repo, &["cargo test"], &envs, None);
    let hits = mock.hits();
    mock.stop();

    assert_eq!(code, 0);
    let envelope: Value = serde_json::from_str(&stdout).expect("envelope");
    assert_eq!(envelope["routing"]["passthrough"], true);
    // Prompt must be echoed verbatim, not rewritten.
    assert_eq!(envelope["payload"]["compiled_prompt"], "cargo test");
    assert_eq!(hits, 0, "passthrough gate must not call the LLM");
}

#[test]
fn sync_tokens_persists_on_a_fresh_repo() {
    // Regression: --sync-tokens used to fail silently when .router/ did not exist
    // yet, so calibration before the first routed turn was a no-op.
    let repo = fixture_repo("pipe-sync-fresh");
    assert!(!std::path::Path::new(&format!("{}/.router", repo)).exists());

    let (stdout, stderr, code) = run(&repo, &["--sync-tokens", "36000"], OFFLINE, None);
    assert_eq!(code, 0, "sync must succeed on a clean repo: {stderr}");
    assert!(stdout.is_empty(), "sync must not write to stdout");

    let state = read_json(&format!("{}/.router/session.json", repo));
    assert_eq!(state["accumulated_tokens"].as_u64().unwrap(), 36000);
    assert_eq!(state["session_id"].as_u64().unwrap(), 1);

    // A malformed count must be rejected rather than silently ignored.
    let (_s, _e, code) = run(&repo, &["--sync-tokens", "not-a-number"], OFFLINE, None);
    assert_eq!(code, 1, "invalid token count should exit non-zero");
}

#[test]
fn mode_a_spawns_harness_and_propagates_exit_code() {
    let repo = fixture_repo("pipe-mode-a");
    // Fake harness writes a marker, asserts env injection, and exits 7.
    let harness = format!("{}/fake_harness.sh", repo);
    std::fs::write(
        &harness,
        "#!/bin/sh\necho \"$ROUTED_SUBAGENT|$ROUTED_SESSION_ACTION|$ROUTED_SKILLS|${1:-}\" > marker.txt\nexit 7\n",
    )
    .expect("write harness");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let envs = [
        ("ROUTER_HARNESS_BIN", harness.as_str()),
        ("ROUTER_LLM_URL", "http://127.0.0.1:1/v1/chat/completions"),
        ("ROUTER_TIMEOUT_MS", "150"),
    ];
    let (stdout, _stderr, code) = run(&repo, &["rust lifetime bug"], &envs, None);
    assert_eq!(code, 7, "router must propagate the harness exit code");
    // Mode A must NOT print the envelope to stdout.
    assert!(stdout.is_empty(), "mode A stdout should be empty, got: {stdout}");

    let marker = std::fs::read_to_string(format!("{}/marker.txt", repo)).expect("marker");
    let mut parts = marker.trim().split('|');
    assert_eq!(parts.next(), Some("rust-architect"));
    assert_eq!(parts.next(), Some("CONTINUE_SESSION"));
    parts.next(); // ROUTED_SKILLS (empty for this trigger-less prompt)
    let prompt = parts.next().unwrap_or("");
    assert!(prompt.contains("<objective>"), "harness received compiled prompt");
}

#[test]
fn registry_cache_round_trips_and_reuses_hash() {
    let repo = fixture_repo("pipe-cache");
    let cache = format!("{}/.router/registry.json", repo);
    run(&repo, &["probe"], OFFLINE, None);
    assert!(
        std::path::Path::new(&cache).exists(),
        "first run must persist the registry cache"
    );
    let first = read_json(&cache)["hash"].as_u64().unwrap();

    // Warm run must reuse (same hash) since no manifest changed.
    run(&repo, &["probe again"], OFFLINE, None);
    let second = read_json(&cache)["hash"].as_u64().unwrap();
    assert_eq!(first, second, "unchanged tree must not bump the xxh3 fingerprint");

    // Adding a manifest must invalidate the cache.
    std::fs::write(
        format!("{}/.agents/new-agent.md", repo),
        "---\nname: new-agent\ndescription: added\ntriggers: [new]\n---\n",
    )
    .unwrap();
    run(&repo, &["probe after add"], OFFLINE, None);
    let third = read_json(&cache)["hash"].as_u64().unwrap();
    assert_ne!(first, third, "added manifest must invalidate the cache hash");

    // And the new agent must be discoverable + routable.
    let entries = read_json(&cache)["entries"]
        .as_object()
        .expect("entries map")
        .clone();
    assert!(entries.contains_key("new-agent"), "registry must pick up new manifest");
    assert!(entries.contains_key("tdd-workflow"), "skill ids must be indexed");
}

==> crates/ext-agent-router/src/compiler.rs <==
//! System 2 XML prompt compiler & subagent selector (spec §4.2.5).
//!
//! Two modes, chosen by configuration:
//! * **Adjudicated** — ask an OpenAI-compatible endpoint to pick the subagent and rewrite
//!   the prompt. Any failure (offline, timeout, malformed JSON) fails open to the local
//!   deterministic compiler, exactly as the legacy router did.
//! * **Deterministic** — compile the raw prompt into a `<s2_compilation>` envelope built
//!   by `s1s2_core::s2::xml_compiler::XmlEnvelopeBuilder`.
//!
//! Both modes keep the legacy `<objective>` / `<constraints>` / `<verification>` blocks so
//! harnesses that grep for them keep working, and both are byte-deterministic for a given
//! input: no clock, no randomness, registry order is the BTreeMap order.

use crate::heuristics::HeuristicCandidate;
use crate::types::{AgentEntry, AgentRegistry, LlmAdjudicatorConfig, PromptInput, RouterConfig};
use s1s2_core::s2::xml_compiler::XmlEnvelopeBuilder;
use serde::{Deserialize, Serialize};

/// Local deterministic compiler: the legacy fallback template.
///
/// Used for a high-confidence System 1 commit, where the routing is already settled and
/// only the prompt needs structuring.
pub fn compile_deterministic(raw_prompt: &str) -> String {
    format!(
        "<objective>\n{}\n</objective>\n<constraints>\n- Follow existing repository patterns and conventions.\n- Do not introduce unrequested dependencies or unrelated refactors.\n</constraints>\n<verification>\n- Run relevant test suites and verify zero compiler/linter errors before completing.\n</verification>",
        raw_prompt.trim()
    )
}

/// Build the full System 2 deliberative envelope: budget header, intent compilation,
/// constraints, candidate field, and handoff state.
#[allow(clippy::too_many_arguments)]
pub fn compile_envelope(
    input: &PromptInput,
    candidate: &HeuristicCandidate,
    registry: &AgentRegistry,
    selected: &str,
    skills: &[AgentEntry],
    saturation_ratio: f32,
    used_tokens: usize,
    limit_tokens: usize,
    handoff_summary: Option<&str>,
) -> String {
    let mut b = XmlEnvelopeBuilder::new();
    b.open("s2_compilation", &[("pathway", "system2")]);

    // Budget header first: System 2 must be able to see why it was called.
    let saturation = format!("{:.3}", saturation_ratio);
    let used = used_tokens.to_string();
    let limit = limit_tokens.to_string();
    let status = if saturation_ratio > s1s2_core::s1::circuit::DEFAULT_HARD_CEILING_RATIO {
        "HALT"
    } else if saturation_ratio >= s1s2_core::s1::circuit::DEFAULT_SOFT_WARNING_RATIO {
        "WARN"
    } else {
        "OK"
    };
    b.empty_tag(
        "context_budget",
        &[
            ("saturation", &saturation),
            ("status", status),
            ("used_tokens", &used),
            ("limit_tokens", &limit),
        ],
    );

    b.open("intent_compilation", &[]);
    b.text_tag("task", &[], input.raw_prompt.trim());
    b.text_tag("objective", &[], input.raw_prompt.trim());
    let role = role_for(selected, registry);
    b.text_tag("agent_role", &[], &role);
    b.text_tag(
        "context_constraints",
        &[("max_saturation", "0.35")],
        "Work within 35% of the model context window. Prefer the smallest diff that satisfies the task.",
    );
    b.close("intent_compilation");

    b.open("candidates", &[("count", &candidate.runner_ups.len().saturating_add(1).to_string())]);
    for entry in candidate.matched_agent.iter().chain(candidate.runner_ups.iter()) {
        b.empty_tag(
            "agent",
            &[("id", &entry.id), ("description", &entry.description)],
        );
    }
    b.close("candidates");

    b.open("constraints", &[]);
    for line in [
        "Follow existing repository patterns and conventions.",
        "Do not introduce unrequested dependencies or unrelated refactors.",
        "Run relevant test suites and verify zero compiler/linter errors before completing.",
    ] {
        b.text_tag("rule", &[], line);
    }
    b.close("constraints");

    b.open("output_contract", &[]);
    b.text_tag("selected_agent", &[], selected);
    for skill in skills {
        b.empty_tag("required_skill", &[("id", &skill.id), ("path", &skill.path)]);
    }
    b.close("output_contract");

    b.text_tag("handoff_state", &[("session_id", &handoff_session_id(&input, selected))], handoff_summary.unwrap_or(""));

    b.close("s2_compilation");
    b.finish()
}

fn handoff_session_id(input: &PromptInput, selected: &str) -> String {
    input
        .session_state
        .as_ref()
        .map(|s| s.session_id.to_string())
        .unwrap_or_else(|| format!("pending:{selected}"))
}

/// Adopt the selected agent's declared role, or a neutral one for `default`.
fn role_for(selected: &str, registry: &AgentRegistry) -> String {
    match registry.get(selected) {
        Some(entry) => format!(
            "Act as `{}`: {}",
            entry.id,
            if entry.description.is_empty() { "no description declared" } else { &entry.description }
        ),
        None => "Act as the default generalist engineering agent.".to_string(),
    }
}

/// Schema the adjudicator must answer with (unchanged from the legacy compiler).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LlmCompilerOutput {
    pub passthrough: bool,
    pub target_subagent: String,
    pub required_skills: Vec<String>,
    pub compiled_prompt: String,
    #[serde(default)]
    pub context_handoff_summary: Option<String>,
}

/// Never let the adjudicator invent capabilities.
pub fn sanitize_hallucinated_ids(decision: &mut LlmCompilerOutput, registry: &AgentRegistry) {
    let valid_agent = registry
        .get(&decision.target_subagent)
        .map(|e| e.kind == "agent")
        .unwrap_or(false);
    if !valid_agent {
        decision.target_subagent = "default".to_string();
    }
    decision.required_skills.retain(|skill_id| {
        registry.get(skill_id).map(|e| e.kind == "skill").unwrap_or(false)
    });
}

fn strip_markdown_fences(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_end_matches("```").trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_end_matches("```").trim()
    } else {
        trimmed
    }
}

/// Serialize the registry into the compact form the adjudicator prompt expects.
fn compact_registry(registry: &AgentRegistry) -> String {
    let compact: Vec<_> = registry
        .values()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "kind": e.kind,
                "description": e.description,
                "triggers": e.triggers,
            })
        })
        .collect();
    serde_json::to_string(&compact).unwrap_or_else(|_| "[]".to_string())
}

/// Ask the configured endpoint to compile and route. Network I/O only happens here, and
/// only on the deliberative path.
pub fn adjudicate(
    input: &PromptInput,
    registry: &AgentRegistry,
    rolling_summary: &str,
    saturation_warning: bool,
    llm: &LlmAdjudicatorConfig,
) -> Result<LlmCompilerOutput, String> {
    if !llm.enabled {
        return Err("adjudicator disabled".to_string());
    }
    let system_prompt = compiler_system_prompt(
        &compact_registry(registry),
        rolling_summary,
        saturation_warning,
    );

    let resp = s1s2_core::s2::llm::chat_completion(
        &llm.url,
        &llm.api_key,
        &llm.model,
        llm.timeout_ms,
        &system_prompt,
        &input.raw_prompt,
    )?;

    let raw_content = s1s2_core::s2::llm::assistant_content(&resp)?;

    serde_json::from_str::<LlmCompilerOutput>(strip_markdown_fences(raw_content))
        .map_err(|e| e.to_string())
}

/// The adjudicator contract. Kept as a standalone function so tests can pin its content.
pub fn compiler_system_prompt(
    registry_json: &str,
    rolling_summary: &str,
    saturation_warning: bool,
) -> String {
    format!(
        "You are a deterministic Prompt Compiler and Subagent Router.
Respond ONLY with a valid JSON object matching this exact schema:
{{
  \"passthrough\": boolean,
  \"target_subagent\": \"string (exact id of an agent from Available Registry, or 'default')\",
  \"required_skills\": [\"string (exact ids of skills from Available Registry)\"],
  \"compiled_prompt\": \"string\",
  \"context_handoff_summary\": \"string or null\"
}}

Available Registry:
{}

Prior Session Summary:
\"{}\"

Session Reset Triggered (>35% context usage): {}

Prompt Engineering Rules for `compiled_prompt`:
1. Transform the user's raw request into a structured specification using XML tags:
   <objective>Clear, concrete engineering goal.</objective>
   <constraints>Guardrails: do not invent dependencies, preserve existing APIs, minimize diff surface.</constraints>
   <verification>Exact test or build commands required to prove correctness.</verification>
2. Select `target_subagent` ONLY from entries with kind=='agent'. If none fit, output \"default\".
3. Select `required_skills` ONLY from entries with kind=='skill'.
4. If Session Reset Triggered is true, compress Prior Session Summary + current intent into `context_handoff_summary` (max 150 words).",
        registry_json, rolling_summary, saturation_warning
    )
}

/// Deterministic tie-break over the System 1 candidate field: prefer the entry whose
/// declared capabilities cover the most prompt tokens, then the lexicographically
/// smallest id so the result never depends on iteration order.
pub fn resolve_ambiguity(
    prompt: &str,
    candidate: &HeuristicCandidate,
) -> Option<AgentEntry> {
    let mut field: Vec<AgentEntry> = Vec::new();
    field.extend(candidate.matched_agent.clone());
    field.extend(candidate.runner_ups.iter().cloned());
    if field.is_empty() {
        return None;
    }

    let tokens: Vec<String> = prompt
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    let coverage = |entry: &AgentEntry| -> usize {
        entry
            .keyword_surface()
            .iter()
            .filter(|keyword| {
                tokens.iter().any(|t| t.contains(keyword.as_str()) || keyword.contains(t.as_str()))
            })
            .count()
    };

    let mut best = field.into_iter().collect::<Vec<_>>();
    // Trim to the highest-scoring tier first so ids are only consulted among true ties.
    let top = best
        .iter()
        .map(coverage)
        .max()
        .unwrap_or(0);
    best.retain(|e| coverage(e) == top);
    best.sort_by(|a, b| a.id.cmp(&b.id));
    best.into_iter().next()
}

/// Whether the escalation was saturation-driven, which the adjudicator prompt reports.
pub fn saturation_driven(config: &RouterConfig, ratio: f32) -> bool {
    let _ = config;
    ratio >= s1s2_core::s1::circuit::DEFAULT_SOFT_WARNING_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heuristics::score_candidate;
    use crate::types::SessionState;

    fn registry() -> AgentRegistry {
        let mut r = AgentRegistry::new();
        for (id, kind, path, triggers) in [
            ("rust-architect", "agent", ".agents/rust-architect.md", vec!["rust", "lifetime"]),
            ("tdd-workflow", "skill", "skills/tdd/SKILL.md", vec!["test", "tdd"]),
        ] {
            r.insert(
                id.to_string(),
                AgentEntry {
                    id: id.to_string(),
                    kind: kind.to_string(),
                    path: path.to_string(),
                    description: format!("{id} description"),
                    triggers: triggers.iter().map(|t| t.to_string()).collect(),
                    name: None,
                    capabilities: Vec::new(),
                    model_tier: None,
                },
            );
        }
        r
    }

    fn input(raw: &str) -> PromptInput {
        PromptInput {
            raw_prompt: raw.to_string(),
            context_tokens_used: 20_000,
            context_window_limit: 200_000,
            session_state: Some(SessionState::default()),
            force_compile: false,
        }
    }

    #[test]
    fn deterministic_compiler_keeps_legacy_tags() {
        let compiled = compile_deterministic("fix the parser\n");
        assert!(compiled.starts_with("<objective>\nfix the parser\n</objective>"), "{compiled}");
        assert!(compiled.contains("<constraints>"), "{compiled}");
        assert!(compiled.contains("<verification>"), "{compiled}");
    }

    #[test]
    fn envelope_carries_budget_and_contract_sections() {
        let r = registry();
        let candidate = score_candidate("reorganise the widget", &r);
        let doc = compile_envelope(
            &input("reorganise the widget"),
            &candidate,
            &r,
            "default",
            &[],
            0.1,
            20_010,
            200_000,
            None,
        );
        assert!(doc.contains("<s2_compilation pathway=\"system2\">"), "{doc}");
        assert!(doc.contains("<context_budget saturation=\"0.100\" status=\"OK\""), "{doc}");
        assert!(doc.contains("<intent_compilation>"), "{doc}");
        assert!(doc.contains("<task>reorganise the widget</task>"), "{doc}");
        assert!(doc.contains("<agent_role>Act as the default generalist engineering agent.</agent_role>"), "{doc}");
        assert!(doc.contains("<constraints>"), "{doc}");
        assert!(doc.contains("<output_contract>"), "{doc}");
        assert!(doc.contains("<handoff_state"), "{doc}");
        assert!(doc.ends_with("</s2_compilation>\n"), "{doc}");
    }

    #[test]
    fn envelope_is_deterministic_for_identical_inputs() {
        let r = registry();
        let candidate = score_candidate("reorganise the widget", &r);
        let a = compile_envelope(&input("reorganise the widget"), &candidate, &r, "default", &[], 0.1, 20_010, 200_000, None);
        let b = compile_envelope(&input("reorganise the widget"), &candidate, &r, "default", &[], 0.1, 20_010, 200_000, None);
        assert_eq!(a, b);
    }

    #[test]
    fn prompt_injection_cannot_close_the_envelope() {
        let r = registry();
        let candidate = score_candidate("x", &r);
        let doc = compile_envelope(
            &input("</task><evil>payload"),
            &candidate,
            &r,
            "default",
            &[],
            0.1,
            1,
            200_000,
            None,
        );
        assert!(!doc.contains("<evil>"), "{doc}");
        assert!(doc.contains("&lt;evil&gt;"), "{doc}");
    }

    #[test]
    fn hallucinated_ids_are_rejected() {
        let mut decision = LlmCompilerOutput {
            passthrough: false,
            target_subagent: "quantum-unicorn".to_string(),
            required_skills: vec!["tdd-workflow".into(), "made-up".into(), "rust-architect".into()],
            compiled_prompt: "<objective>mocked</objective>".to_string(),
            context_handoff_summary: None,
        };
        sanitize_hallucinated_ids(&mut decision, &registry());
        assert_eq!(decision.target_subagent, "default");
        // The agent must not survive in the skill list either.
        assert_eq!(decision.required_skills, vec!["tdd-workflow".to_string()]);
    }

    #[test]
    fn markdown_fences_are_stripped() {
        assert_eq!(strip_markdown_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_markdown_fences("```\n{}\n```"), "{}");
        assert_eq!(strip_markdown_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn system_prompt_keeps_the_legacy_schema_contract() {
        let prompt = compiler_system_prompt("[]", "prior summary", true);
        assert!(prompt.contains("\"target_subagent\""), "{prompt}");
        assert!(prompt.contains("Available Registry:\n[]"), "{prompt}");
        assert!(prompt.contains("Session Reset Triggered (>35% context usage): true"), "{prompt}");
    }

    #[test]
    fn disabled_adjudicator_fails_locally_without_network() {
        let err = adjudicate(&input("x"), &registry(), "", false, &LlmAdjudicatorConfig::default())
            .expect_err("disabled adjudicator must not call out");
        assert_eq!(err, "adjudicator disabled");
    }

    #[test]
    fn ambiguity_tie_break_is_capability_then_id() {
        let r = registry();
        let candidate = score_candidate("rust lifetime", &r);
        let resolved = resolve_ambiguity("rust lifetime", &candidate).expect("one winner");
        assert_eq!(resolved.id, "rust-architect");
        assert!(resolve_ambiguity("nothing matches", &HeuristicCandidate::default()).is_none());
    }
}

==> crates/ext-agent-router/src/heuristics.rs <==
//! System 1 lexical / trigger matching for subagents (spec §4.2.3).
//!
//! Pure and deterministic: lowercase the prompt, tokenize it, then score each declared
//! capability. No allocations per candidate beyond the score vector, no I/O, no clock.
//! The confidence model is documented at `score_candidate`.

use crate::types::{AgentEntry, AgentRegistry};
use serde::{Deserialize, Serialize};

/// Multiplier for a trigger phrase that appears verbatim in the prompt.
const EXACT_TRIGGER_WEIGHT: f32 = 2.0;
/// Multiplier for a capability keyword hitting a prompt token.
const CAPABILITY_WEIGHT: f32 = 1.0;
/// Bonus when the entry id itself appears in the prompt (an explicit `use rust-architect`).
const ID_MATCH_BONUS: f32 = 3.0;

/// Fast-path output carried into the gate.
///
/// Every field has a serde default so a partially-populated legacy payload (the shape the
/// old dispatcher logged) still deserializes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeuristicCandidate {
    /// Best-matched `agent` entry, if any scored above zero.
    #[serde(default)]
    pub matched_agent: Option<AgentEntry>,
    /// Runner-up agents, in descending score order — System 2 uses these to break ties.
    #[serde(default)]
    pub runner_ups: Vec<AgentEntry>,
    /// Skills whose keywords appear in the prompt, in registry order.
    #[serde(default)]
    pub matched_skills: Vec<AgentEntry>,
    /// `0.0..=1.0`. How decisively `matched_agent` beat the field.
    #[serde(default)]
    pub confidence: f32,
    /// Trigger phrases that actually matched, for traceability.
    #[serde(default)]
    pub matched_triggers: Vec<String>,
    /// Number of agents scoring above the ambiguity floor.
    #[serde(default)]
    pub candidate_count: usize,
}

impl HeuristicCandidate {
    pub fn best_agent(&self) -> Option<&AgentEntry> {
        self.matched_agent.as_ref()
    }

    pub fn skill_paths(&self) -> Vec<String> {
        self.matched_skills.iter().map(|e| e.path.clone()).collect()
    }

    pub fn skill_ids(&self) -> Vec<String> {
        self.matched_skills.iter().map(|e| e.id.clone()).collect()
    }
}

/// Split a prompt into lowercase word tokens, dropping punctuation.
fn tokenize(prompt: &str) -> Vec<String> {
    prompt
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Raw score for one entry: id bonus + trigger phrases + capability keyword hits.
fn raw_score(entry: &AgentEntry, lowered: &str, tokens: &[String]) -> (f32, Vec<String>) {
    let mut score = 0.0f32;
    let mut hits = Vec::new();

    if !entry.id.is_empty() && lowered.contains(&entry.id.to_lowercase()) {
        score += ID_MATCH_BONUS;
        hits.push(entry.id.clone());
    }

    for trigger in &entry.triggers {
        let needle = trigger.to_lowercase();
        if needle.is_empty() {
            continue;
        }
        if lowered.contains(&needle) {
            score += EXACT_TRIGGER_WEIGHT * needle.split_whitespace().count() as f32;
            hits.push(trigger.clone());
        }
    }

    for keyword in &entry.capabilities {
        let needle = keyword.to_lowercase();
        if tokens.iter().any(|t| *t == needle) {
            score += CAPABILITY_WEIGHT;
            hits.push(keyword.clone());
        }
    }

    (score, hits)
}

/// Score the whole registry and derive confidence from the winning margin.
pub fn score_candidate(prompt: &str, registry: &AgentRegistry) -> HeuristicCandidate {
    let lowered = prompt.to_lowercase();
    let tokens = tokenize(prompt);

    let mut agents: Vec<(f32, AgentEntry, Vec<String>)> = Vec::new();
    let mut skills: Vec<AgentEntry> = Vec::new();

    for entry in registry.values() {
        let (score, hits) = raw_score(entry, &lowered, &tokens);
        if entry.is_skill() {
            if score > 0.0 {
                skills.push(entry.clone());
            }
        } else if score > 0.0 {
            agents.push((score, entry.clone(), hits));
        }
    }

    // Descending score, then id for deterministic tie-breaking.
    agents.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.id.cmp(&b.1.id)));

    // Any keyword above the floor is a live candidate for the ambiguity count.
    let floor = agents.first().map(|(s, _, _)| *s * 0.5).unwrap_or(0.0);
    let candidate_count = agents.iter().filter(|(s, _, _)| *s >= floor.max(0.5)).count();

    let Some((best_score, best, best_hits)) = agents.first().cloned() else {
        return HeuristicCandidate {
            matched_agent: None,
            runner_ups: Vec::new(),
            matched_skills: skills,
            confidence: 0.0,
            matched_triggers: Vec::new(),
            candidate_count: 0,
        };
    };

    let second_score = agents.get(1).map(|(s, _, _)| *s).unwrap_or(0.0);
    // Winning-margin confidence: an uncontested match scores 1.0, a dead tie scores 0.0,
    // and anything in between reflects how much clearer the winner was.
    let confidence = if best_score <= 0.0 {
        0.0
    } else {
        ((best_score - second_score) / best_score).clamp(0.0, 1.0)
    };

    HeuristicCandidate {
        matched_agent: Some(best),
        runner_ups: agents.into_iter().skip(1).map(|(_, e, _)| e).collect(),
        matched_skills: skills,
        confidence,
        matched_triggers: best_hits,
        candidate_count,
    }
}

/// Trivial commands never need compilation: short shell/test invocations pass through.
pub fn is_trivial_passthrough(raw_prompt: &str) -> bool {
    let trimmed = raw_prompt.trim();
    let word_count = trimmed.split_whitespace().count();
    let command_prefixes = ["cargo ", "git ", "npm ", "pytest", "ls", "cd ", "/clear", "/help"];
    word_count <= 4 && command_prefixes.iter().any(|p| trimmed.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentEntry;

    fn registry() -> AgentRegistry {
        let mut r = AgentRegistry::new();
        let agent = AgentEntry {
            id: "rust-architect".into(),
            kind: "agent".into(),
            path: ".agents/rust-architect.md".into(),
            description: "Systems architect".into(),
            triggers: vec!["rust".into(), "borrow".into(), "lifetime".into()],
            name: Some("rust-architect".into()),
            capabilities: Vec::new(),
            model_tier: None,
        };
        let skill = AgentEntry {
            id: "tdd-workflow".into(),
            kind: "skill".into(),
            path: "skills/tdd/SKILL.md".into(),
            description: "Red-Green-Refactor".into(),
            triggers: vec!["test".into(), "tdd".into()],
            name: None,
            capabilities: Vec::new(),
            model_tier: None,
        };
        let other = AgentEntry {
            id: "db-migrator".into(),
            kind: "agent".into(),
            path: ".agents/db-migrator.md".into(),
            description: "SQL migrations".into(),
            triggers: Vec::new(),
            name: Some("db-migrator".into()),
            capabilities: vec!["sql".into(), "migration".into()],
            model_tier: None,
        };
        r.insert(agent.id.clone(), agent);
        r.insert(skill.id.clone(), skill);
        r.insert(other.id.clone(), other);
        r
    }

    #[test]
    fn explicit_id_mention_is_decisive() {
        let candidate = score_candidate("activate rust-architect to fix the lifetime bug", &registry());
        assert_eq!(candidate.matched_agent.as_ref().unwrap().id, "rust-architect");
        assert!(candidate.confidence >= 0.85, "confidence {}", candidate.confidence);
        assert_eq!(candidate.candidate_count, 1);
        assert!(candidate.matched_triggers.iter().any(|t| t == "rust" || t == "lifetime"));
        // No skill keyword appears in this prompt, so the skill surface stays empty.
        assert!(candidate.skill_ids().is_empty(), "{:?}", candidate.skill_ids());

        let with_skill =
            score_candidate("activate rust-architect and write the tdd harness", &registry());
        assert_eq!(with_skill.skill_ids(), vec!["tdd-workflow".to_string()]);
    }

    #[test]
    fn no_match_scores_zero() {
        let candidate = score_candidate("reorganise the widget factory then maybe", &registry());
        assert!(candidate.matched_agent.is_none());
        assert_eq!(candidate.confidence, 0.0);
        assert_eq!(candidate.candidate_count, 0);
    }

    #[test]
    fn tied_agents_report_low_confidence_and_two_candidates() {
        let mut r = registry();
        let twin = AgentEntry {
            id: "rust-architect-2".into(),
            kind: "agent".into(),
            path: ".agents/rust-architect-2.md".into(),
            description: "Second".into(),
            triggers: vec!["rust".into(), "borrow".into(), "lifetime".into()],
            name: None,
            capabilities: Vec::new(),
            model_tier: None,
        };
        r.insert(twin.id.clone(), twin);
        let candidate = score_candidate("rust lifetime", &r);
        assert!(candidate.confidence < 0.85, "tied field must not commit: {}", candidate.confidence);
        assert!(candidate.candidate_count >= 2, "{}", candidate.candidate_count);
    }

    #[test]
    fn capability_keywords_are_matched_as_tokens() {
        let candidate = score_candidate("apply this sql change", &registry());
        assert_eq!(candidate.matched_agent.as_ref().unwrap().id, "db-migrator");
    }

    #[test]
    fn matching_is_pure_and_repeatable() {
        let r = registry();
        let a = score_candidate("fix rust borrow checker", &r);
        let b = score_candidate("fix rust borrow checker", &r);
        assert_eq!(a.confidence, b.confidence);
        assert_eq!(a.matched_agent, b.matched_agent);
    }

    #[test]
    fn trivial_commands_pass_through() {
        assert!(is_trivial_passthrough("cargo test"));
        assert!(is_trivial_passthrough("git status"));
        assert!(!is_trivial_passthrough("cargo test and then refactor the parser layer"));
        assert!(!is_trivial_passthrough("implement a trie"));
    }
}

==> crates/ext-agent-router/src/lib.rs <==
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

==> crates/ext-agent-router/src/registry.rs <==
//! Capability discovery for Extension #1 (spec §4.2.1), built on `s1s2_core::s1::cache`.
//!
//! Parsing is unchanged from the legacy router: zero-dependency YAML frontmatter with a
//! Markdown-header fallback, plus the `.agent.md` / `.prompt.md` / `.instructions.md`
//! double-extension stripping that existing repositories rely on for their ids.

use crate::types::{AgentEntry, AgentRegistry};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Parse one manifest into an `AgentEntry`.
///
/// `kind` comes from the directory that declared the file, never from the file itself.
pub fn parse_manifest(repo_root: &Path, file_path: &Path, kind: &str, content: &str) -> AgentEntry {
    let rel_path = file_path
        .strip_prefix(repo_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/");

    let default_id = if file_path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
        file_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-skill")
            .to_string()
    } else {
        let raw_stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-agent");
        raw_stem
            .strip_suffix(".agent")
            .or_else(|| raw_stem.strip_suffix(".instructions"))
            .or_else(|| raw_stem.strip_suffix(".prompt"))
            .unwrap_or(raw_stem)
            .to_string()
    };

    let mut id = default_id;
    let mut description = String::new();
    let mut triggers = Vec::new();
    let mut capabilities = Vec::new();
    let mut model_tier: Option<String> = None;

    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) == Some("---") {
        for line in lines.by_ref() {
            let trimmed = line.trim();
            if trimmed == "---" {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim();
                let val = v.trim().trim_matches('"').trim_matches('\'');
                match key {
                    "name" | "id" if !val.is_empty() => id = val.to_string(),
                    "description" if !val.is_empty() => description = val.to_string(),
                    "triggers" => triggers = parse_inline_list(val),
                    "capabilities" => capabilities = parse_inline_list(val),
                    "model_tier" | "model" if !val.is_empty() => model_tier = Some(val.to_string()),
                    _ => {}
                }
            }
        }
    }

    if description.is_empty() {
        description = content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim().starts_with("---"))
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        if description.len() > 240 {
            let mut end = 240;
            while end > 0 && !description.is_char_boundary(end) {
                end -= 1;
            }
            description.truncate(end);
        }
    }

    let name = if file_path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
        None
    } else {
        Some(id.clone())
    };

    AgentEntry {
        id,
        kind: kind.to_string(),
        path: rel_path,
        description,
        triggers,
        name,
        capabilities,
        model_tier,
    }
}

fn parse_inline_list(val: &str) -> Vec<String> {
    let cleaned = val.trim_start_matches('[').trim_end_matches(']');
    cleaned
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Walk the declared directories and build the capability map.
pub fn build_registry(repo_root: &Path, search_dirs: &[(String, String)]) -> Result<AgentRegistry, String> {
    let mut entries = AgentRegistry::new();
    for file_path in discover_files(repo_root, search_dirs) {
        let Ok(content) = fs::read_to_string(&file_path) else { continue };
        let Some(kind) = kind_for(&file_path, repo_root, search_dirs) else { continue };
        let entry = parse_manifest(repo_root, &file_path, kind, &content);
        entries.insert(entry.id.clone(), entry);
    }
    Ok(entries)
}

/// Every Markdown manifest under the declared directories, in stable sorted order.
pub fn discover_files(repo_root: &Path, search_dirs: &[(String, String)]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for (dir, _) in search_dirs {
        let full_dir = repo_root.join(dir);
        if !full_dir.exists() {
            continue;
        }
        files.extend(
            WalkDir::new(&full_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .map(|e| e.into_path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md")),
        );
    }
    files.sort();
    files.dedup();
    files
}

/// Which declared directory owns this file — the innermost match wins, so `skills/tdd`
/// does not shadow `.claude/skills`.
fn kind_for<'a>(
    file_path: &Path,
    repo_root: &Path,
    search_dirs: &'a [(String, String)],
) -> Option<&'a str> {
    let absolute = normalize(file_path.to_path_buf());
    let mut best: Option<(&str, usize)> = None;
    for (dir, kind) in search_dirs {
        let root = normalize(repo_root.join(dir));
        if absolute.starts_with(&root) {
            let depth = root.matches('/').count();
            if best.map(|(_, d)| depth >= d).unwrap_or(true) {
                best = Some((kind.as_str(), depth));
            }
        }
    }
    best.map(|(kind, _)| kind)
}

fn normalize(path: PathBuf) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_frontmatter_accurately() {
        let md = "---\nname: sec-auditor\ndescription: Audits Rust unsafe blocks\ntriggers: [security, unsafe, audit]\n---\n# Body";
        let entry = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.agents/sec.md"),
            "agent",
            md,
        );
        assert_eq!(entry.id, "sec-auditor");
        assert_eq!(entry.description, "Audits Rust unsafe blocks");
        assert_eq!(entry.triggers, vec!["security", "unsafe", "audit"]);
        assert!(entry.is_agent());
    }

    #[test]
    fn reads_capabilities_and_model_tier() {
        let md = "---\nname: dba\ndescription: Database work\ncapabilities: [sql, migrations]\nmodel_tier: sonnet\n---\n";
        let entry = parse_manifest(Path::new("/r"), Path::new("/r/.agents/dba.md"), "agent", md);
        assert_eq!(entry.capabilities, vec!["sql", "migrations"]);
        assert_eq!(entry.model_tier.as_deref(), Some("sonnet"));
        // Capability keywords widen the System 1 match surface.
        assert!(entry.keyword_surface().contains(&"sql".to_string()));
    }

    #[test]
    fn strips_double_extensions_when_name_omitted() {
        let md = "---\ndescription: Custom agent without explicit name\ntriggers: [rust]\n---\n# Body";
        let agent = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/agents/rust-architect.agent.md"),
            "agent",
            md,
        );
        assert_eq!(agent.id, "rust-architect");

        let instr = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/instructions/coding-style.instructions.md"),
            "skill",
            md,
        );
        assert_eq!(instr.id, "coding-style");

        let prompt = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/prompts/refactor.prompt.md"),
            "skill",
            md,
        );
        assert_eq!(prompt.id, "refactor");

        let skill = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/skills/tdd/SKILL.md"),
            "skill",
            md,
        );
        assert_eq!(skill.id, "tdd");
        assert_eq!(skill.name, None, "SKILL.md keeps the legacy no-alias shape");
    }

    #[test]
    fn truncates_long_multibyte_description_without_panicking() {
        let md = format!("# Title\n{}", "é".repeat(300));
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/no-front.md"), "agent", &md);
        assert!(entry.description.len() <= 240);
        assert!(entry.description.is_char_boundary(entry.description.len()));
    }

    #[test]
    fn kind_is_taken_from_the_owning_directory() {
        let repo = std::env::temp_dir().join(format!("ext-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(repo.join(".agents")).unwrap();
        fs::create_dir_all(repo.join("skills/tdd")).unwrap();
        let agent_file = repo.join(".agents/arch.md");
        let skill_file = repo.join("skills/tdd/SKILL.md");
        fs::write(&agent_file, "---\nname: arch\n---\n").unwrap();
        fs::write(&skill_file, "---\nname: tdd-workflow\n---\n").unwrap();

        let dirs = vec![
            (".agents".to_string(), "agent".to_string()),
            ("skills".to_string(), "skill".to_string()),
        ];
        let registry = build_registry(&repo, &dirs).unwrap();
        assert_eq!(registry.get("arch").unwrap().kind, "agent");
        assert_eq!(registry.get("tdd-workflow").unwrap().kind, "skill");
        assert_eq!(registry.get("tdd-workflow").unwrap().path, "skills/tdd/SKILL.md");
        let _ = fs::remove_dir_all(&repo);
    }
}

==> crates/ext-agent-router/src/types.rs <==
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

==> crates/ext-agent-router/tests/pathways.rs <==
//! Spec §6 Phase 4: System 1 / Gate / System 2 pathway tests for Extension #1.
//!
//! These drive `CognitiveEngine<AgentRoutingDomain>` in-process (no subprocess), which
//! is the contract spec §3.4 defines. Legacy stdout/hook JSON compatibility is covered
//! separately by `bins/agent-router/tests/`.

use ext_agent_router::heuristics::HeuristicCandidate;
use ext_agent_router::types::{PromptInput, RouterConfig};
use ext_agent_router::AgentRoutingDomain;
use s1s2_core::domain::{CircuitTrip, EscalationReason};
use s1s2_core::s1::circuit::CircuitBreakerConfig;
use s1s2_core::{CognitiveEngine, ExecutionPathway};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Fresh repo seeded with one agent + one skill manifest, unique per test tag.
fn fixture_repo(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("s1s2-ext-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".agents")).expect("mkdir .agents");
    std::fs::create_dir_all(root.join("skills/tdd")).expect("mkdir skills");
    std::fs::write(
        root.join(".agents/rust-architect.md"),
        "---\nname: rust-architect\ndescription: Systems architect\ntriggers: [rust, borrow, lifetime]\n---\nBody\n",
    )
    .expect("write agent");
    std::fs::write(
        root.join("skills/tdd/SKILL.md"),
        "---\nname: tdd-workflow\ndescription: Red-Green-Refactor\ntriggers: [test, tdd]\n---\nBody\n",
    )
    .expect("write skill");
    root
}

/// Zero base overhead so the spec's 75,000/200,000 == 37.5% figure is exact.
fn engine_for(root: &Path) -> CognitiveEngine<AgentRoutingDomain> {
    let config = RouterConfig::standalone(root.to_path_buf());
    CognitiveEngine::new(config, CircuitBreakerConfig::default())
}

fn prompt(raw: &str, used: usize, limit: usize) -> PromptInput {
    PromptInput {
        raw_prompt: raw.to_string(),
        context_tokens_used: used,
        context_window_limit: limit,
        session_state: None,
        force_compile: false,
    }
}

#[test]
fn test_s1_fast_commit_under_budget() {
    let root = fixture_repo("fast-commit");
    let mut engine = engine_for(&root);

    // Cold run builds the registry so the timed run below is warm.
    let cold = engine
        .process(&prompt("activate rust-architect to fix the lifetime bug", 20_000, 200_000))
        .expect("pipeline must not error");
    assert!(!cold.cache_hit);

    // 10% saturation (20,000 / 200,000) plus an explicit trigger => deterministic match.
    let started = std::time::Instant::now();
    let env = engine
        .process(&prompt("activate rust-architect to fix the lifetime bug", 20_000, 200_000))
        .expect("pipeline must not error");
    let wall = started.elapsed();

    assert!(
        matches!(env.pathway, ExecutionPathway::System1Fast),
        "expected fast commit, got {:?}",
        env.pathway
    );
    assert!(env.cache_hit);
    assert_eq!(env.decision.selected_agent, "rust-architect");
    assert_eq!(env.decision.target_subagent_path, ".agents/rust-architect.md");
    // No skill triggers appear in this prompt, so none are attached.
    assert!(env.decision.required_skills.is_empty(), "{:?}", env.decision.required_skills);
    assert!(!env.decision.passthrough);
    assert!(!env.decision.circuit_tripped);
    assert_eq!(env.decision.saturation_ratio, 0.1);
    assert_eq!(env.decision.action, "CONTINUE_SESSION");
    assert_eq!(env.decision.handoff_artifact, None);
    assert_eq!(env.decision.pathway_metadata, Some(ExecutionPathway::System1Fast));

    // System 2 was never invoked: no deliberative envelope in the compiled prompt.
    assert!(!env.decision.compiled_prompt.contains("<s2_compilation"));
    assert!(!env.decision.compiled_prompt.contains("<intent_compilation>"));
    // The deterministic System 1 template still produces the legacy `<objective>` spec.
    assert!(env.decision.compiled_prompt.contains("<objective>"));

    // Spec §6.4.1: < 2ms warm, and no LLM adjudication was attempted.
    assert!(env.latency_micros < 2_000, "engine latency {}us", env.latency_micros);
    assert!(wall < Duration::from_millis(2), "wall {wall:?}");

    // The System 1 candidate is carried through for continuity.
    let candidate: HeuristicCandidate =
        serde_json::from_value(serde_json::json!({
            "matched_agent": null, "confidence": 0.0, "matched_triggers": []
        }))
        .expect("HeuristicCandidate must accept the legacy JSON shape");
    assert_eq!(candidate.confidence, 0.0);
}

#[test]
fn test_s1_to_s2_escalation_on_ambiguity() {
    let root = fixture_repo("escalate");
    let mut engine = engine_for(&root);
    engine.process(&prompt("warm", 1_000, 200_000)).expect("warm registry");

    // Zero deterministic matches at 10% saturation => LowConfidence escalation.
    let env = engine
        .process(&prompt("reorganise the widget factory then maybe", 20_000, 200_000))
        .expect("pipeline must not error");

    match &env.pathway {
        ExecutionPathway::System2Escalated(EscalationReason::LowConfidence { score, threshold }) => {
            assert!(*score < 0.85, "score {score} must be below threshold");
            assert_eq!(*threshold, 0.85);
        }
        other => panic!("expected LowConfidence escalation, got {other:?}"),
    }

    // System 2 produced a valid, deterministic XML-compiled prompt.
    let compiled = &env.decision.compiled_prompt;
    assert!(compiled.contains("<s2_compilation"), "{compiled}");
    assert!(compiled.contains("<context_budget saturation=\"0.100\" status=\"OK\""), "{compiled}");
    assert!(compiled.contains("<intent_compilation"), "{compiled}");
    assert!(compiled.contains("<task>"), "{compiled}");
    assert!(compiled.contains("<agent_role>"), "{compiled}");
    assert!(compiled.contains("<constraints>"), "{compiled}");
    assert!(compiled.contains("<output_contract>"), "{compiled}");
    assert!(compiled.contains("<handoff_state"), "{compiled}");
    assert!(compiled.contains("reorganise the widget factory then maybe"), "{compiled}");
    assert_eq!(env.decision.pathway_metadata.as_ref(), Some(&env.pathway));

    // Escalation is deterministic: the same input compiles to identical bytes.
    let again = engine
        .process(&prompt("reorganise the widget factory then maybe", 20_000, 200_000))
        .expect("second compile");
    assert_eq!(again.decision.compiled_prompt, *compiled);

    // Ambiguous candidates: two agents share the winning trigger score.
    std::fs::write(
        root.join(".agents/rust-architect-2.md"),
        "---\nname: rust-architect-2\ndescription: Second systems architect\ntriggers: [rust]\n---\nBody\n",
    )
    .expect("write second agent");
    let env = engine
        .process(&prompt("rust", 20_000, 200_000))
        .expect("ambiguous pipeline");
    match &env.pathway {
        ExecutionPathway::System2Escalated(reason) => {
            assert!(
                matches!(
                    reason,
                    EscalationReason::LowConfidence { .. }
                        | EscalationReason::AmbiguousCandidates { candidate_count: 2 }
                ),
                "{reason:?}"
            );
        }
        other => panic!("expected escalation on ambiguity, got {other:?}"),
    }

    // Saturation warning escalates even a deterministic match (>= 30%, below the ceiling).
    let env = engine
        .process(&prompt("activate rust-architect now", 62_000, 200_000))
        .expect("escalation on saturation warning");
    match &env.pathway {
        ExecutionPathway::System2Escalated(EscalationReason::ContextSaturationWarning { ratio }) => {
            assert!(*ratio > 0.31 && *ratio < 0.32, "{ratio}");
        }
        other => panic!("expected saturation-warning escalation, got {other:?}"),
    }
    // System 2 compacts: the budget header must report WARN.
    assert!(env.decision.compiled_prompt.contains("status=\"WARN\""));

    // force_compile is an explicit System 2 request.
    let mut forced = prompt("cargo test", 1_000, 200_000);
    forced.force_compile = true;
    let env = engine.process(&forced).expect("force_compile escalation");
    assert!(matches!(
        env.pathway,
        ExecutionPathway::System2Escalated(EscalationReason::ExplicitSystem2Request)
    ));
}

#[test]
fn test_circuit_breaker_halts_at_35_percent() {
    let root = fixture_repo("halt");
    let mut engine = engine_for(&root);
    engine.process(&prompt("warm", 1_000, 200_000)).expect("warm registry");

    // 75,000 / 200,000 = 37.5% > 35% hard ceiling.
    let env = engine
        .process(&prompt("implement rust parser", 75_000, 200_000))
        .expect("halt must still return a decision");

    match &env.pathway {
        ExecutionPathway::CircuitHalted(CircuitTrip::SaturationExceeded { current_ratio, max_ratio }) => {
            // used = 75,000 ledger + prompt estimate, so the ratio sits just above 0.375.
            assert!(*current_ratio > 0.375 && *current_ratio < 0.376, "{current_ratio}");
            assert_eq!(*max_ratio, 0.35);
        }
        other => panic!("expected saturation halt, got {other:?}"),
    }

    assert!(env.decision.circuit_tripped);
    assert_eq!(env.decision.action, "START_NEW_SESSION");
    assert_eq!(env.decision.selected_agent, "default");
    assert_eq!(env.decision.saturation_ratio, 0.375);
    assert!(!env.decision.compiled_prompt.contains("<s2_compilation"));

    // System 2 is bypassed; the halt payload is a compact breaker notice instead.
    assert!(
        env.decision.compiled_prompt.contains("<circuit_breaker_tripped"),
        "{}",
        env.decision.compiled_prompt
    );

    // A compaction handoff artifact preserving session state is emitted.
    let artifact = env
        .decision
        .handoff_artifact
        .clone()
        .expect("halt must emit a handoff artifact");
    assert!(artifact.contains("<circuit_breaker_tripped max=\"0.35\""), "{artifact}");
    assert!(artifact.contains("<session_envelope session_id=\"1\""), "{artifact}");
    assert!(artifact.contains("</handoff_summary>"), "{artifact}");
    assert_eq!(env.decision.pathway_metadata.as_ref(), Some(&env.pathway));
}

#[test]
fn test_mtime_cache_invalidation() {
    let root = fixture_repo("mtime");
    let mut engine = engine_for(&root);

    let first = engine.process(&prompt("probe", 1_000, 200_000)).expect("cold run");
    assert!(!first.cache_hit, "cold run must build the registry");

    let second = engine.process(&prompt("probe again", 1_000, 200_000)).expect("warm run");
    assert!(second.cache_hit, "warm run must reuse the xxh64 mtime cache");

    // Bump the manifest mtime without changing its size.
    let target = root.join(".agents/rust-architect.md");
    let meta = std::fs::metadata(&target).expect("metadata");
    let bumped = meta
        .modified()
        .unwrap_or_else(|_| SystemTime::now())
        + Duration::from_secs(5);
    std::fs::File::open(&target)
        .expect("open")
        .set_modified(bumped)
        .expect("set_modified");

    let third = engine
        .process(&prompt("probe after touch", 1_000, 200_000))
        .expect("invalidated run");
    assert!(!third.cache_hit, "mtime change must invalidate the cache");

    let fourth = engine
        .process(&prompt("probe warm again", 1_000, 200_000))
        .expect("warm run after invalidation");
    assert!(fourth.cache_hit);

    // A brand-new manifest invalidates the cache and becomes routable.
    std::fs::write(
        root.join(".agents/new-agent.md"),
        "---\nname: new-agent\ndescription: added\ntriggers: [new]\n---\n",
    )
    .expect("write new agent");
    let fifth = engine.process(&prompt("probe after add", 1_000, 200_000)).expect("run");
    assert!(!fifth.cache_hit);

    let sixth = engine
        .process(&prompt("activate new-agent now", 1_000, 200_000))
        .expect("warm run after add");
    assert!(sixth.cache_hit);
    assert!(matches!(sixth.pathway, ExecutionPathway::System1Fast));
    assert_eq!(sixth.decision.selected_agent, "new-agent");

    // The on-disk snapshot keeps the legacy `{hash, entries}` schema.
    let cache_path = root.join(".router/registry.json");
    let header: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cache_path).expect("cache header"))
            .expect("cache json");
    assert!(header["hash"].is_number(), "{header}");
    assert!(header["entries"].is_object(), "{header}");
    assert!(header["entries"].get("rust-architect").is_some(), "{header}");
}

==> crates/s1s2-core/src/domain.rs <==
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

==> crates/s1s2-core/src/gate.rs <==
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

==> crates/s1s2-core/src/harness/cli.rs <==
//! Subprocess wrapper and JSON stdout dispatcher (spec §5, generalized from `dispatcher.rs`).
//!
//! Two dispatch modes:
//! * **Mode A (`spawn`)** — hand the compiled prompt to a downstream CLI harness,
//!   passing routing metadata through `ROUTED_*` environment variables, and propagate
//!   the child's exit code.
//! * **Mode B (`emit_json`)** — print the payload to stdout as pretty JSON. Diagnostics
//!   must never share stdout, so harnesses can pipe it straight into a parser.

use serde::Serialize;
use std::io::Write;
use std::process::{Command, Stdio};

/// One `ROUTED_*` environment binding for the child harness.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchEnv {
    pub key: String,
    pub value: String,
}

impl DispatchEnv {
    pub fn new(key: &str, value: &str) -> Self {
        Self { key: key.to_string(), value: value.to_string() }
    }
}

/// Everything Mode A needs: the binary to spawn, the prompt to pass, and the env block.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub program: String,
    pub prompt: String,
    pub args: Vec<String>,
    pub env: Vec<DispatchEnv>,
}

impl SpawnRequest {
    pub fn new(program: &str, prompt: &str) -> Self {
        Self { program: program.to_string(), prompt: prompt.to_string(), args: Vec::new(), env: Vec::new() }
    }

    pub fn arg(mut self, arg: &str) -> Self {
        self.args.push(arg.to_string());
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        if !value.is_empty() {
            self.env.push(DispatchEnv::new(key, value));
        }
        self
    }
}

/// Run the child harness with inherited stdio, returning its exit code.
///
/// A spawn failure is reported as `Err` so the caller can fail open rather than
/// silently pretending the child ran.
pub fn spawn(request: &SpawnRequest) -> Result<i32, String> {
    let mut command = Command::new(&request.program);
    command
        .envs(request.env.iter().map(|e| (e.key.as_str(), e.value.as_str())))
        .args(&request.args)
        .arg(&request.prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command
        .status()
        .map_err(|e| format!("Failed to spawn harness '{}': {}", request.program, e))?;
    Ok(status.code().unwrap_or(0))
}

/// Mode B: serialize `payload` as pretty JSON and write it to stdout in one call.
pub fn emit_json<T: Serialize>(payload: &T) -> Result<(), String> {
    let body = serde_json::to_string_pretty(payload).map_err(|e| e.to_string())?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{body}").map_err(|e| e.to_string())?;
    handle.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Serialize without printing, for callers that want the bytes (MCP text content).
pub fn to_json_string<T: Serialize>(payload: &T) -> Result<String, String> {
    serde_json::to_string(payload).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_builder_skips_empty_values() {
        let req = SpawnRequest::new("harness", "prompt")
            .env("ROUTED_SUBAGENT", "rust-architect")
            .env("ROUTED_SKILLS", "");
        assert_eq!(req.env.len(), 1, "empty values must not be injected");
        assert_eq!(req.env[0].key, "ROUTED_SUBAGENT");
        assert_eq!(req.env[0].value, "rust-architect");
    }

    #[test]
    fn spawn_reports_missing_program_as_error() {
        let req = SpawnRequest::new("/nonexistent/agent-router-harness-binary", "p");
        let err = spawn(&req).expect_err("missing program must fail");
        assert!(err.contains("Failed to spawn"), "{err}");
    }

    #[test]
    fn spawn_propagates_child_exit_code() {
        let req = SpawnRequest::new("true", "p");
        assert_eq!(spawn(&req).expect("true must exit 0"), 0);
        let req = SpawnRequest::new("sh", "p").arg("-c").arg("exit 7");
        // `sh -c "exit 7" p`: the trailing prompt arg is $0, so the exit code is 7.
        assert_eq!(spawn(&req).expect("child exit code"), 7);
    }

    #[test]
    fn json_helpers_round_trip() {
        let payload = serde_json::json!({"routing": {"target_subagent": "rust-architect"}});
        let compact = to_json_string(&payload).expect("serialize");
        assert!(compact.contains("\"target_subagent\""), "{compact}");
        assert!(!compact.contains('\n'), "compact form must be single-line");
    }
}

==> crates/s1s2-core/src/harness/mcp.rs <==
//! Zero-dependency MCP (Model Context Protocol) JSON-RPC 2.0 stdio server.
//!
//! Framing is newline-delimited JSON-RPC over stdin/stdout, which is what every
//! harness-side MCP client speaks today. Only four request methods are implemented:
//! `initialize`, `tools/list`, `tools/call`, and `ping`. Notifications get no response.
//!
//! The server is transport-only: tools are supplied by the caller, so the core stays
//! independent of any domain.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC error carried out of a tool handler.
#[derive(Debug, Clone)]
pub struct McpError {
    pub code: i64,
    pub message: String,
}

impl McpError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }
}

/// A tool exposed to MCP clients. `handler` receives the decoded `arguments` object.
pub struct McpTool {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub handler: Box<dyn FnMut(Value) -> Result<Value, McpError>>,
}

impl McpTool {
    pub fn new(
        name: &'static str,
        description: &'static str,
        input_schema: Value,
        handler: impl FnMut(Value) -> Result<Value, McpError> + 'static,
    ) -> Self {
        Self { name, description, input_schema, handler: Box::new(handler) }
    }
}

/// Newline-delimited JSON-RPC 2.0 server over stdio.
pub struct McpServer {
    server_name: String,
    server_version: String,
    tools: Vec<McpTool>,
}

impl McpServer {
    pub fn new(server_name: &str, server_version: &str) -> Self {
        Self {
            server_name: server_name.to_string(),
            server_version: server_version.to_string(),
            tools: Vec::new(),
        }
    }

    pub fn tool(mut self, tool: McpTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Read requests from stdin and answer on stdout until EOF.
    pub fn serve_stdio(&mut self) -> io::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(response) = self.handle_line(trimmed) {
                writeln!(out, "{response}")?;
                out.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one framed request. `None` means "send nothing" (a notification).
    pub fn handle_line(&mut self, raw: &str) -> Option<String> {
        let request: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => return Some(error(Value::Null, -32700, &format!("Parse error: {e}"))),
        };

        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        // JSON-RPC 2.0: a request with no `id` is a notification and is never answered,
        // whatever its method.
        let id = request.get("id").cloned()?;

        let payload = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": self.server_name,
                    "version": self.server_version,
                },
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": self.tool_descriptors() }),
            "tools/call" => return Some(self.call_tool(&id, &request)),
            "" => return Some(error(id, -32600, "Invalid Request")),
            other => {
                return Some(error(
                    id,
                    -32601,
                    &format!("Method not found: {other}"),
                ))
            }
        };

        Some(response(id, payload))
    }

    fn tool_descriptors(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect()
    }

    fn call_tool(&mut self, id: &Value, request: &Value) -> String {
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        if name.is_empty() {
            return error(id.clone(), -32602, "Missing tools/call params.name");
        }

        let index = match self.tools.iter().position(|t| t.name == name) {
            Some(i) => i,
            None => {
                return error(id.clone(), -32602, &format!("Unknown tool: {name}"))
            }
        };

        let (payload, is_error) = match (self.tools[index].handler)(arguments) {
            Ok(payload) => (payload, false),
            Err(err) => (json!({ "error": err.message }), true),
        };

        let text = if is_error {
            payload["error"].as_str().unwrap_or_default().to_string()
        } else {
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| String::new())
        };

        response(
            id.clone(),
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error,
            }),
        )
    }
}

fn response(id: Value, payload: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": payload,
    }))
    .expect("response is always serializable")
}

fn error(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
    .expect("error is always serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_server() -> McpServer {
        McpServer::new("test-server", "2.0.0").tool(McpTool::new(
            "route_intent",
            "Echo the arguments back.",
            json!({
                "type": "object",
                "properties": { "raw_prompt": { "type": "string" } },
                "required": ["raw_prompt"],
            }),
            |args| Ok(json!({ "echoed": args })),
        ))
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).expect("valid json")
    }

    #[test]
    fn initialize_announces_capabilities_and_server_info() {
        let mut server = echo_server();
        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
            )
            .expect("initialize must be answered");
        let value = parse(&reply);
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(value["result"]["capabilities"]["tools"].is_object());
        assert_eq!(value["result"]["serverInfo"]["name"], "test-server");
    }

    #[test]
    fn notifications_are_never_answered() {
        let mut server = echo_server();
        assert_eq!(
            server.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
    }

    #[test]
    fn tools_list_exposes_schemas() {
        let mut server = echo_server();
        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .expect("list reply");
        let value = parse(&reply);
        let tools = value["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "route_intent");
        assert_eq!(tools[0]["inputSchema"]["required"][0], "raw_prompt");
    }

    #[test]
    fn tools_call_wraps_payload_in_text_content() {
        let mut server = echo_server();
        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"route_intent","arguments":{"raw_prompt":"fix rust"}}}"#,
            )
            .expect("call reply");
        let value = parse(&reply);
        assert_eq!(value["id"], 3);
        assert_eq!(value["result"]["isError"], false);
        let text = value["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("fix rust"), "{text}");
    }

    #[test]
    fn tool_failure_is_a_result_not_a_jsonrpc_error() {
        let mut server = McpServer::new("s", "1").tool(McpTool::new(
            "boom",
            "Always fails.",
            json!({"type": "object"}),
            |_| Err(McpError::invalid_params("raw_prompt required")),
        ));
        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"boom","arguments":{}}}"#,
            )
            .expect("reply");
        let value = parse(&reply);
        assert_eq!(value["result"]["isError"], true);
        assert!(value["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("raw_prompt required"));
    }

    #[test]
    fn unknown_method_and_malformed_frames_use_jsonrpc_errors() {
        let mut server = echo_server();
        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":5,"method":"tools/nope"}"#)
            .expect("error reply");
        let value = parse(&reply);
        assert_eq!(value["error"]["code"], -32601);

        let reply = server.handle_line("{not json").expect("parse error reply");
        assert_eq!(parse(&reply)["error"]["code"], -32700);

        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{}}"#)
            .expect("missing name");
        assert_eq!(parse(&reply)["error"]["code"], -32602);

        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"ghost"}}"#,
            )
            .expect("unknown tool");
        assert_eq!(parse(&reply)["error"]["code"], -32602);
    }

    #[test]
    fn ping_is_answered_with_an_empty_result() {
        let mut server = echo_server();
        let reply = server.handle_line(r#"{"jsonrpc":"2.0","id":8,"method":"ping"}"#).unwrap();
        assert_eq!(parse(&reply)["result"], json!({}));
    }
}

==> crates/s1s2-core/src/harness/mod.rs <==
//! Execution harness adapters: how a finished decision reaches the outside world.

pub mod cli;
pub mod mcp;

==> crates/s1s2-core/src/lib.rs <==
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

==> crates/s1s2-core/src/s1/cache.rs <==
//! Generic `xxh64` mtime-backed state cache (spec §3.2.2).
//!
//! The registry is the expensive part of System 1: walking the tree and parsing every
//! frontmatter block. Fingerprinting `path + mtime + size` lets a warm run deserialize
//! the previous snapshot instead of re-reading the files.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;
use xxhash_rust::xxh64::xxh64;

/// On-disk header key holding the fingerprint, alongside `payload_key`.
pub const HASH_KEY: &str = "hash";

/// Deterministic fingerprint over watched directories. Entries are sorted so the digest
/// does not depend on filesystem iteration order.
pub fn fingerprint(watched_dirs: &[PathBuf], extensions: &[&str]) -> (u64, Vec<PathBuf>) {
    let mut lines: Vec<String> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();

    for dir in watched_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in WalkDir::new(dir).follow_links(false).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !is_wanted(path, extensions) {
                continue;
            }
            let Ok(meta) = fs::metadata(path) else { continue };
            let nanos = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            lines.push(format!("{}:{}:{}|", path.display(), nanos, meta.len()));
            files.push(path.to_path_buf());
        }
    }

    lines.sort();
    files.sort();
    files.dedup();
    (xxh64(lines.join("").as_bytes(), 0), files)
}

fn is_wanted(path: &Path, extensions: &[&str]) -> bool {
    if !path.is_file() {
        return false;
    }
    // An empty filter means "any file", which is what non-file-backed domains want.
    if extensions.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| extensions.contains(&ext))
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct MtimeCacheState<T> {
    pub payload: T,
    pub hash: u64,
    pub cache_hit: bool,
}

/// Reusable cache handle; `T` is a domain's `RegistryState`.
///
/// The file layout is `{"hash": <xxh64>, "<payload_key>": <T>}`. Extension #1 chooses
/// `payload_key = "entries"`, which reproduces the legacy `.router/registry.json` schema
/// exactly, so existing harnesses keep reading it unchanged.
pub struct MtimeCache<T: Serialize + DeserializeOwned> {
    cache_file: PathBuf,
    payload_key: String,
    extensions: Vec<String>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> MtimeCache<T> {
    pub fn new(cache_file: PathBuf, payload_key: &str, extensions: &[&str]) -> Self {
        Self {
            cache_file,
            payload_key: payload_key.to_string(),
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn cache_file(&self) -> &Path {
        &self.cache_file
    }

    /// Read the cached payload when the fingerprint still matches; otherwise rebuild,
    /// persist, and report `cache_hit == false`.
    pub fn load_or_build(
        &mut self,
        watched_dirs: &[PathBuf],
        build: impl FnOnce(&[PathBuf]) -> Result<T, String>,
    ) -> Result<MtimeCacheState<T>, String> {
        let exts: Vec<&str> = self.extensions.iter().map(|s| s.as_str()).collect();
        let (hash, files) = fingerprint(watched_dirs, &exts);

        if let Ok(raw) = fs::read_to_string(&self.cache_file) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                if value.get(HASH_KEY).and_then(|h| h.as_u64()) == Some(hash) {
                    if let Some(payload) = value.get(&self.payload_key) {
                        if let Ok(typed) = serde_json::from_value::<T>(payload.clone()) {
                            return Ok(MtimeCacheState { payload: typed, hash, cache_hit: true });
                        }
                    }
                }
            }
        }

        let payload = build(&files)?;
        self.write(hash, &payload);
        Ok(MtimeCacheState { payload, hash, cache_hit: false })
    }

    /// Force the next `load_or_build` to rebuild.
    pub fn invalidate(&mut self) {
        let _ = fs::remove_file(&self.cache_file);
    }

    fn write(&self, hash: u64, payload: &T) {
        let Ok(body) = serde_json::to_value(payload) else { return };
        let mut document = serde_json::Map::new();
        document.insert(HASH_KEY.to_string(), serde_json::Value::Number(hash.into()));
        document.insert(self.payload_key.clone(), body);

        if let Some(parent) = self.cache_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(
            &self.cache_file,
            serde_json::to_string_pretty(&document).unwrap_or_default(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("s1s2-cache-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fingerprint_is_order_independent_and_size_sensitive() {
        let dir = temp_dir("fp");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.md"), "one").unwrap();
        fs::write(dir.join("sub/b.md"), "two").unwrap();

        let dirs = vec![dir.clone()];
        let (h1, files) = fingerprint(&dirs, &["md"]);
        assert_eq!(files.len(), 2);
        let (h2, _) = fingerprint(&dirs, &["md"]);
        assert_eq!(h1, h2, "unchanged tree must fingerprint identically");

        fs::write(dir.join("sub/b.md"), "two-and-a-bit-longer").unwrap();
        let (h3, _) = fingerprint(&dirs, &["md"]);
        assert_ne!(h1, h3, "size change must invalidate");

        let (_h4, none) = fingerprint(&dirs, &["rs"]);
        assert!(none.is_empty(), "extension filter must apply");

        let (_h5, any) = fingerprint(&dirs, &[]);
        assert_eq!(any.len(), 2, "empty filter accepts any file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_round_trips_and_rebuilds_on_invalidate() {
        let dir = temp_dir("rt");
        fs::write(dir.join("x.md"), "hello").unwrap();
        let mut cache: MtimeCache<BTreeMap<String, String>> =
            MtimeCache::new(dir.join("state.json"), "payload", &["md"]);

        let builds = Cell::new(0usize);
        let mut build = |_: &[PathBuf]| {
            builds.set(builds.get() + 1);
            let mut m = BTreeMap::new();
            m.insert("x".to_string(), "hello".to_string());
            Ok(m)
        };

        let first = cache.load_or_build(&[dir.clone()], &mut build).unwrap();
        assert!(!first.cache_hit);
        let second = cache.load_or_build(&[dir.clone()], &mut build).unwrap();
        assert!(second.cache_hit);
        assert_eq!(builds.get(), 1, "warm run must not rebuild");

        cache.invalidate();
        let third = cache.load_or_build(&[dir.clone()], &mut build).unwrap();
        assert!(!third.cache_hit);
        assert_eq!(third.payload, second.payload);
        assert_eq!(builds.get(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_only_change_invalidates_without_size_change() {
        let dir = temp_dir("mtime");
        let file = dir.join("x.md");
        fs::write(&file, "hello").unwrap();
        let dirs = vec![dir.clone()];

        let (h1, _) = fingerprint(&dirs, &["md"]);
        let meta = fs::metadata(&file).unwrap();
        let bumped = meta.modified().unwrap() + Duration::from_secs(3);
        fs::File::open(&file).unwrap().set_modified(bumped).unwrap();
        let (h2, _) = fingerprint(&dirs, &["md"]);
        assert_ne!(h1, h2, "mtime bump alone must invalidate");
        let _ = fs::remove_dir_all(&dir);
    }
}

==> crates/s1s2-core/src/s1/circuit.rs <==
//! Generalized token/resource budgeter and saturation circuit breaker (spec §3.2.1).
//!
//! The estimator is the legacy calibrated heuristic, kept byte-for-byte identical so
//! every existing saturation assertion (35% ceiling, handoff sizing, banner percentages)
//! produces the same numbers after the refactor.

use crate::domain::CircuitTrip;

/// Hard ceiling: never let a turn project above 35% of the model context window.
pub const DEFAULT_HARD_CEILING_RATIO: f32 = 0.35;
/// Soft warning: escalate to System 2 so it can compact before the ceiling is hit.
pub const DEFAULT_SOFT_WARNING_RATIO: f32 = 0.30;
/// Window assumed when a caller supplies no limit.
pub const DEFAULT_CONTEXT_WINDOW: usize = 200_000;

/// Zero-dependency token estimator calibrated for code + English
/// (~3.6 chars/token plus a punctuation/newline weighting). No BPE table, no network.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let char_count = text.chars().count();
    let base_tokens = (char_count as f64 / 3.6).ceil() as usize;
    let punct_or_op = text
        .chars()
        .filter(|c| c.is_ascii_punctuation() || *c == '\n')
        .count();
    base_tokens + (punct_or_op / 3)
}

/// Round a saturation ratio the way the legacy envelope reported it (3 decimals).
pub fn round_ratio(ratio: f32) -> f32 {
    (ratio * 1000.0).round() / 1000.0
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CircuitBreakerConfig {
    pub max_saturation_ratio: f32,
    pub soft_warning_ratio: f32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_saturation_ratio: DEFAULT_HARD_CEILING_RATIO,
            soft_warning_ratio: DEFAULT_SOFT_WARNING_RATIO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetReading {
    pub used: usize,
    pub limit: usize,
    /// `used / limit`, clamped to a sane floor when the limit is missing.
    pub saturation_ratio: f32,
}

impl BudgetReading {
    pub fn exceeds_hard_ceiling(&self, cfg: CircuitBreakerConfig) -> bool {
        self.saturation_ratio > cfg.max_saturation_ratio
    }

    pub fn exceeds_soft_warning(&self, cfg: CircuitBreakerConfig) -> bool {
        self.saturation_ratio >= cfg.soft_warning_ratio
    }
}

/// Reusable guard evaluated before System 1 runs, so a saturated context never
/// reaches the (expensive) deliberative path.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreaker {
    pub config: CircuitBreakerConfig,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self { config: CircuitBreakerConfig::default() }
    }
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self { config }
    }

    pub fn read(&self, used: usize, limit: usize) -> BudgetReading {
        let limit = if limit == 0 { DEFAULT_CONTEXT_WINDOW } else { limit };
        BudgetReading {
            used,
            limit,
            saturation_ratio: (used as f32) / (limit as f32),
        }
    }

    /// `Some(trip)` when a hard invariant is violated.
    pub fn trip(&self, reading: &BudgetReading) -> Option<CircuitTrip> {
        if reading.exceeds_hard_ceiling(self.config) {
            Some(CircuitTrip::SaturationExceeded {
                current_ratio: reading.saturation_ratio,
                max_ratio: self.config.max_saturation_ratio,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimator_counts_punctuation_weight() {
        assert_eq!(estimate_tokens(""), 0);
        // 36 chars -> ceil(10) = 10 tokens, no punctuation.
        assert_eq!(estimate_tokens(&"a".repeat(36)), 10);
        let with_punct = "a\n".repeat(18); // 36 chars, 18 newlines
        assert_eq!(estimate_tokens(&with_punct), 10 + 6);
    }

    #[test]
    fn breaker_trips_strictly_above_the_ceiling() {
        let breaker = CircuitBreaker::default();
        let exactly = breaker.read(70_000, 200_000);
        assert_eq!(exactly.saturation_ratio, 0.35);
        assert!(breaker.trip(&exactly).is_none(), "0.35 is not > 0.35");

        let over = breaker.read(75_000, 200_000);
        assert_eq!(over.saturation_ratio, 0.375);
        assert_eq!(
            breaker.trip(&over),
            Some(CircuitTrip::SaturationExceeded { current_ratio: 0.375, max_ratio: 0.35 })
        );
    }

    #[test]
    fn soft_warning_fires_before_the_ceiling() {
        let breaker = CircuitBreaker::default();
        let warn = breaker.read(62_000, 200_000);
        assert!(warn.exceeds_soft_warning(breaker.config));
        assert!(!warn.exceeds_hard_ceiling(breaker.config));
        let clean = breaker.read(20_000, 200_000);
        assert_eq!(clean.saturation_ratio, 0.1);
        assert!(!clean.exceeds_soft_warning(breaker.config));
    }

    #[test]
    fn zero_limit_falls_back_to_the_default_window() {
        let breaker = CircuitBreaker::default();
        let reading = breaker.read(20_000, 0);
        assert_eq!(reading.limit, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn round_ratio_matches_legacy_three_decimal_format() {
        assert_eq!(round_ratio(0.35234375), 0.352);
        assert_eq!(round_ratio(0.375001), 0.375);
    }
}

==> crates/s1s2-core/src/s1/mod.rs <==
//! System 1: fast, reactive, deterministic subsystems (spec §3.2).

pub mod cache;
pub mod circuit;

==> crates/s1s2-core/src/s2/handoff.rs <==
//! Session continuity & handoff artifacts (spec §3.3.2).
//!
//! A halt is only useful if the next session can pick up where this one stopped, so
//! the ledger, the pathway taken, and a summary travel together as a typed envelope
//! that a downstream harness can deserialize or a human can read as Markdown.

use crate::domain::{CircuitTrip, EscalationReason};
use crate::s2::xml_compiler::XmlEnvelopeBuilder;
use serde::{Deserialize, Serialize};

/// Which cognitive pathway produced the last decision. Serialized as a stable snake-case
/// slug so persisted ledgers stay readable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathwayKind {
    #[serde(rename = "system1_fast")]
    System1Fast,
    #[serde(rename = "system2_deliberative")]
    System2Deliberative,
    #[serde(rename = "circuit_breaker_halt")]
    CircuitBreakerHalt,
}

impl Default for PathwayKind {
    fn default() -> Self {
        PathwayKind::System1Fast
    }
}

impl PathwayKind {
    pub fn slug(self) -> &'static str {
        match self {
            PathwayKind::System1Fast => "system1_fast",
            PathwayKind::System2Deliberative => "system2_deliberative",
            PathwayKind::CircuitBreakerHalt => "circuit_breaker_halt",
        }
    }

    pub fn legacy_action(self) -> &'static str {
        match self {
            PathwayKind::CircuitBreakerHalt => "START_NEW_SESSION",
            _ => "CONTINUE_SESSION",
        }
    }
}

/// The continuity record a domain keeps between turns (Extension #1's `SessionState`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLedger {
    pub session_id: u64,
    #[serde(default)]
    pub host_session_id: Option<String>,
    #[serde(default)]
    pub accumulated_tokens: usize,
    #[serde(default)]
    pub rolling_summary: String,
    #[serde(default)]
    pub turn_count: u64,
    #[serde(default)]
    pub last_pathway: PathwayKind,
}

impl Default for SessionLedger {
    fn default() -> Self {
        Self {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 0,
            rolling_summary: String::new(),
            turn_count: 0,
            last_pathway: PathwayKind::default(),
        }
    }
}

/// Generic session envelope (spec §3.3.2): identity + budget + pathway + summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnvelope<T> {
    pub session_id: u64,
    pub turn_count: u64,
    pub cumulative_tokens: usize,
    pub last_pathway: PathwayKind,
    pub handoff_summary: String,
    pub domain_state: T,
}

impl<T> SessionEnvelope<T> {
    pub fn new(ledger: &SessionLedger, domain_state: T) -> Self {
        Self {
            session_id: ledger.session_id,
            turn_count: ledger.turn_count,
            cumulative_tokens: ledger.accumulated_tokens,
            last_pathway: ledger.last_pathway,
            handoff_summary: ledger.rolling_summary.clone(),
            domain_state,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> SessionEnvelope<U> {
        SessionEnvelope {
            session_id: self.session_id,
            turn_count: self.turn_count,
            cumulative_tokens: self.cumulative_tokens,
            last_pathway: self.last_pathway,
            handoff_summary: self.handoff_summary,
            domain_state: f(self.domain_state),
        }
    }
}

/// Why continuity was handed off, so the next session can render it faithfully.
#[derive(Debug, Clone, PartialEq)]
pub enum HandoffCause {
    CircuitTripped(CircuitTrip),
    Escalated(EscalationReason),
}

/// Deterministic handoff payload for downstream execution harnesses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffArtifact {
    pub session_id: u64,
    pub turn_count: u64,
    pub cumulative_tokens: usize,
    pub last_pathway: PathwayKind,
    pub saturation_ratio: f32,
    pub summary: String,
}

/// Render the Markdown/XML artifact a fresh session reads on its first turn.
pub fn render_handoff_artifact(
    ledger: &SessionLedger,
    cause: &HandoffCause,
    saturation_ratio: f32,
    max_saturation_ratio: f32,
) -> String {
    let mut b = XmlEnvelopeBuilder::new();

    match cause {
        HandoffCause::CircuitTripped(trip) => b.open(
            "circuit_breaker_tripped",
            &[
                ("max", &format!("{:.2}", max_saturation_ratio)),
                ("trip", &trip.to_string()),
                ("saturation", &format!("{:.3}", saturation_ratio)),
            ],
        ),
        HandoffCause::Escalated(reason) => b.open(
            "escalation_handoff",
            &[
                ("reason", &reason.to_string()),
                ("saturation", &format!("{:.3}", saturation_ratio)),
            ],
        ),
    };

    b.open(
        "session_envelope",
        &[
            ("session_id", &ledger.session_id.to_string()),
            ("turn_count", &ledger.turn_count.to_string()),
            ("cumulative_tokens", &ledger.accumulated_tokens.to_string()),
            ("last_pathway", ledger.last_pathway.slug()),
        ],
    );
    b.text_tag("handoff_summary", &[], &summary_or_placeholder(ledger));
    b.close("session_envelope");
    b.close(match cause {
        HandoffCause::CircuitTripped(_) => "circuit_breaker_tripped",
        HandoffCause::Escalated(_) => "escalation_handoff",
    });
    b.finish()
}

fn summary_or_placeholder(ledger: &SessionLedger) -> String {
    if ledger.rolling_summary.trim().is_empty() {
        "None recorded.".to_string()
    } else {
        ledger.rolling_summary.clone()
    }
}

/// Load a persisted ledger, defaulting when the file is missing or unreadable so a
/// fresh workspace still routes.
pub fn load_ledger(path: &std::path::Path) -> SessionLedger {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_ledger(path: &std::path::Path, ledger: &SessionLedger) -> std::io::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(ledger)?)
}

/// Commit one turn to the token ledger, rolling the session forward when the ceiling was
/// breached and keeping the rolling summary bounded to the last ~600 characters.
pub fn commit_turn(
    ledger: &mut SessionLedger,
    ceiling_breached: bool,
    pathway: PathwayKind,
    raw_prompt: &str,
    compiled_prompt: &str,
    handoff_summary: Option<String>,
    token_estimate: fn(&str) -> usize,
) {
    let compiled_tokens = token_estimate(compiled_prompt);
    ledger.turn_count += 1;
    ledger.last_pathway = pathway;

    if ceiling_breached {
        ledger.session_id += 1;
        let handoff = handoff_summary.unwrap_or_default();
        ledger.accumulated_tokens = token_estimate(&handoff) + compiled_tokens;
        ledger.rolling_summary = handoff;
        return;
    }

    ledger.accumulated_tokens += compiled_tokens;
    let snippet: String = raw_prompt
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    if snippet.is_empty() {
        return;
    }
    if !ledger.rolling_summary.is_empty() {
        ledger.rolling_summary.push_str(" -> ");
    }
    ledger.rolling_summary.push_str(&snippet);
    if ledger.rolling_summary.len() > 600 {
        let mut start = ledger.rolling_summary.len() - 600;
        while start < ledger.rolling_summary.len() && !ledger.rolling_summary.is_char_boundary(start) {
            start += 1;
        }
        ledger.rolling_summary = ledger.rolling_summary[start..].to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> SessionLedger {
        SessionLedger {
            session_id: 3,
            host_session_id: Some("host-1".into()),
            accumulated_tokens: 75_000,
            rolling_summary: "design ast node structs".into(),
            turn_count: 7,
            last_pathway: PathwayKind::System2Deliberative,
        }
    }

    #[test]
    fn artifact_preserves_state_and_ceiling() {
        let doc = render_handoff_artifact(
            &ledger(),
            &HandoffCause::CircuitTripped(CircuitTrip::SaturationExceeded {
                current_ratio: 0.375,
                max_ratio: 0.35,
            }),
            0.375,
            0.35,
        );
        assert!(doc.starts_with("<circuit_breaker_tripped max=\"0.35\""), "{doc}");
        assert!(doc.contains("<session_envelope session_id=\"3\" turn_count=\"7\""), "{doc}");
        assert!(doc.contains("cumulative_tokens=\"75000\""), "{doc}");
        assert!(doc.contains("last_pathway=\"system2_deliberative\""), "{doc}");
        assert!(doc.contains("<handoff_summary>design ast node structs</handoff_summary>"), "{doc}");
        assert!(doc.ends_with("</circuit_breaker_tripped>\n"), "{doc}");
    }

    #[test]
    fn empty_summary_gets_a_placeholder() {
        let mut l = ledger();
        l.rolling_summary = String::new();
        let doc = render_handoff_artifact(
            &l,
            &HandoffCause::Escalated(EscalationReason::ExplicitSystem2Request),
            0.31,
            0.35,
        );
        assert!(doc.starts_with("<escalation_handoff reason=\"explicit-system2-request\""), "{doc}");
        assert!(doc.contains("None recorded."), "{doc}");
    }

    #[test]
    fn envelope_maps_domain_state_and_legacy_action() {
        let env = SessionEnvelope::new(&ledger(), 42usize);
        assert_eq!(env.session_id, 3);
        assert_eq!(env.cumulative_tokens, 75_000);
        assert_eq!(env.handoff_summary, "design ast node structs");
        assert_eq!(env.map(|n| n.to_string()).domain_state, "42");
        assert_eq!(PathwayKind::CircuitBreakerHalt.legacy_action(), "START_NEW_SESSION");
        assert_eq!(PathwayKind::System1Fast.legacy_action(), "CONTINUE_SESSION");
    }

    #[test]
    fn ledger_json_is_backward_compatible() {
        let json = serde_json::to_string(&ledger()).unwrap();
        assert!(json.contains("\"host_session_id\":\"host-1\""), "{json}");
        let round: SessionLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(round, ledger());
        // A legacy ledger without the v2 fields must still deserialize.
        let legacy = r#"{"session_id":2,"accumulated_tokens":5,"rolling_summary":"x"}"#;
        let parsed: SessionLedger = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.turn_count, 0);
        assert_eq!(parsed.last_pathway, PathwayKind::System1Fast);
    }
}

==> crates/s1s2-core/src/s2/llm.rs <==
//! Optional System 2 external adjudicator transport (feature = `llm`).
//!
//! Network I/O lives in exactly one place in the framework, behind one cargo feature, so
//! an extension that never escalates to a remote model compiles with no HTTP stack at all.
//! The endpoint contract is plain OpenAI-compatible JSON over `ureq` — no async runtime,
//! no OpenSSL, no C bindings.

use std::time::Duration;

/// POST one chat-completion request and return the parsed response body.
///
/// `temperature` is pinned to 0: a deliberative adjudicator that varies run to run would
/// break System 2's determinism guarantee.
pub fn chat_completion(
    url: &str,
    api_key: &str,
    model: &str,
    timeout_ms: u64,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<serde_json::Value, String> {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let body = serde_json::json!({
        "model": model,
        "temperature": 0.0,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_prompt }
        ]
    });

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build();

    let mut request = agent.post(url).set("Content-Type", "application/json");
    if !api_key.is_empty() {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }

    request
        .send_json(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())
}

/// Pull `choices[0].message.content` out of a chat-completion response.
pub fn assistant_content(resp: &serde_json::Value) -> Result<&str, String> {
    resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "Missing choices[0].message.content".to_string())
}

==> crates/s1s2-core/src/s2/mod.rs <==
//! System 2: slow, deliberative, compilative subsystems (spec §3.3).

pub mod handoff;
pub mod xml_compiler;

#[cfg(feature = "llm")]
pub mod llm;

==> crates/s1s2-core/src/s2/xml_compiler.rs <==
//! Deterministic XML prompt & context structurer (spec §3.3.1).
//!
//! System 2 output must be reproducible: same input, same bytes. This builder writes
//! into one `String` buffer (no per-node allocation), always emits attributes in the
//! order supplied, and escapes text and attribute values strictly, so raw user prompts
//! can never break out of the envelope or inject tags.

use std::fmt::Write as _;

/// Escape the five predefined XML entities plus tab/CR/LF in text content.
///
/// Newlines are kept literal inside element bodies (they are valid XML) but escaped in
/// attribute values, where a raw newline would be normalized away by any parser.
pub fn escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Sanitize an attribute value: escape entities and collapse control characters.
pub fn sanitize_attr(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Builds a well-formed XML document into a single reusable buffer.
#[derive(Debug, Default, Clone)]
pub struct XmlEnvelopeBuilder {
    buf: String,
    indent: usize,
}

impl XmlEnvelopeBuilder {
    pub fn new() -> Self {
        Self { buf: String::with_capacity(512), indent: 0 }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self { buf: String::with_capacity(capacity), indent: 0 }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn pad(&mut self) {
        if self.indent > 0 {
            self.buf.push_str(&"  ".repeat(self.indent));
        }
    }

    /// `<name a="1" b="2" />`
    pub fn empty_tag(&mut self, name: &str, attrs: &[(&str, &str)]) -> &mut Self {
        self.pad();
        let _ = write!(self.buf, "<{}", escape_name(name));
        for (k, v) in attrs {
            let _ = write!(self.buf, " {}=\"{}\"", escape_name(k), sanitize_attr(v));
        }
        self.buf.push_str(" />\n");
        self
    }

    /// `<name ...>body</name>` — body is escaped, and empty bodies self-close.
    pub fn text_tag(&mut self, name: &str, attrs: &[(&str, &str)], body: &str) -> &mut Self {
        if body.is_empty() {
            return self.empty_tag(name, attrs);
        }
        self.pad();
        self.write_open_tag(name, attrs);
        self.buf.push('>');
        self.buf.push_str(&escape_text(body));
        self.buf.push_str("</");
        self.buf.push_str(&escape_name(name));
        self.buf.push_str(">\n");
        self
    }

    fn write_open_tag(&mut self, name: &str, attrs: &[(&str, &str)]) {
        let _ = write!(self.buf, "<{}", escape_name(name));
        for (k, v) in attrs {
            let _ = write!(self.buf, " {}=\"{}\"", escape_name(k), sanitize_attr(v));
        }
    }

    /// `<name ...>` followed by nested content, increasing the indent level.
    pub fn open(&mut self, name: &str, attrs: &[(&str, &str)]) -> &mut Self {
        self.pad();
        self.write_open_tag(name, attrs);
        self.buf.push_str(">\n");
        self.indent += 1;
        self
    }

    /// `</name>` closing the most recent `open`.
    pub fn close(&mut self, name: &str) -> &mut Self {
        self.indent = self.indent.saturating_sub(1);
        self.pad();
        let _ = write!(self.buf, "</{}>\n", escape_name(name));
        self
    }

    /// Raw line, used for domain-authored markup that is already escaped.
    pub fn raw_line(&mut self, line: &str) -> &mut Self {
        self.pad();
        self.buf.push_str(line);
        self.buf.push('\n');
        self
    }

    pub fn finish(&self) -> String {
        self.buf.clone()
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

fn escape_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' { c } else { '_' })
        .collect()
}

/// The spec-mandated System 2 envelope header.
pub fn context_budget(saturation_ratio: f32, limit: usize, used: usize) -> String {
    let mut b = XmlEnvelopeBuilder::new();
    b.empty_tag(
        "context_budget",
        &[
            ("saturation", &format!("{:.3}", saturation_ratio)),
            ("status", budget_status(saturation_ratio)),
            ("used_tokens", &used.to_string()),
            ("limit_tokens", &limit.to_string()),
        ],
    );
    b.finish()
}

fn budget_status(saturation_ratio: f32) -> &'static str {
    if saturation_ratio > crate::s1::circuit::DEFAULT_HARD_CEILING_RATIO {
        "HALT"
    } else if saturation_ratio >= crate::s1::circuit::DEFAULT_SOFT_WARNING_RATIO {
        "WARN"
    } else {
        "OK"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_body_entities_strictly() {
        assert_eq!(
            escape_text(r#"<script> & "quotes" 'single'"#),
            "&lt;script&gt; &amp; &quot;quotes&quot; &apos;single&apos;"
        );
    }

    #[test]
    fn attribute_sanitizer_neutralizes_breakouts_and_control_chars() {
        assert_eq!(
            sanitize_attr("a\" /><evil> \n\ttab\u{1}"),
            "a&quot; /&gt;&lt;evil&gt;   tab"
        );
    }

    #[test]
    fn injection_attempt_stays_inside_the_envelope() {
        let mut b = XmlEnvelopeBuilder::new();
        b.text_tag("task", &[], "</task><injected>payload");
        let doc = b.finish();
        assert!(!doc.contains("<injected>"), "{doc}");
        assert!(doc.contains("&lt;/task&gt;&lt;injected&gt;"), "{doc}");
    }

    #[test]
    fn nested_open_close_indents_and_closes() {
        let mut b = XmlEnvelopeBuilder::new();
        b.open("intent_compilation", &[("by", "s2")]);
        b.text_tag("task", &[], "do it");
        b.close("intent_compilation");
        let doc = b.finish();
        assert!(doc.starts_with("<intent_compilation by=\"s2\">\n"), "{doc}");
        assert!(doc.contains("  <task>do it</task>\n"), "{doc}");
        assert!(doc.ends_with("</intent_compilation>\n"), "{doc}");
    }

    #[test]
    fn empty_body_self_closes() {
        let mut b = XmlEnvelopeBuilder::new();
        b.text_tag("handoff_state", &[("session_id", "7")], "");
        assert_eq!(b.finish(), "<handoff_state session_id=\"7\" />\n");
    }

    #[test]
    fn context_budget_reports_status_band() {
        assert!(context_budget(0.1, 200_000, 20_000).contains("status=\"OK\""));
        assert!(context_budget(0.31, 200_000, 62_000).contains("status=\"WARN\""));
        assert!(context_budget(0.375, 200_000, 75_000).contains("status=\"HALT\""));
        assert!(context_budget(0.1, 200_000, 20_000).contains("saturation=\"0.100\""));
    }

    #[test]
    fn invalid_tag_names_are_fenced_not_injected() {
        let mut b = XmlEnvelopeBuilder::new();
        b.text_tag("bad name><x", &[], "v");
        assert!(b.finish().contains("<bad_name__x>"), "{}", b.as_str());
    }
}

==> crates/s1s2-core/tests/mock_domain.rs <==
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
````
