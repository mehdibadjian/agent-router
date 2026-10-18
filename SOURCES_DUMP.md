````rust
==> src/budget.rs <==
use crate::types::SessionState;
use std::fs;
use std::path::Path;

pub const HARD_CEILING_RATIO: f64 = 0.35;

pub struct BudgetEvaluation {
    pub saturation_ratio: f64,
    pub must_reset_session: bool,
    pub prompt_tokens: usize,
}

/// Zero-dependency BPE token estimator calibrated for code + English (~3.6 chars/token + punctuation weight)
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

pub fn load_session(session_file: &Path) -> SessionState {
    fs::read_to_string(session_file)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_session(session_file: &Path, state: &SessionState) -> std::io::Result<()> {
    let serialized = serde_json::to_string_pretty(state)?;
    fs::write(session_file, serialized)
}

pub fn evaluate_budget(
    session: &SessionState,
    raw_prompt: &str,
    base_overhead_tokens: usize,
    max_window_tokens: usize,
) -> BudgetEvaluation {
    let prompt_tokens = estimate_tokens(raw_prompt);
    let projected_total = base_overhead_tokens + session.accumulated_tokens + prompt_tokens;
    let max_safe = max_window_tokens.max(1);
    let saturation_ratio = (projected_total as f64) / (max_safe as f64);

    BudgetEvaluation {
        saturation_ratio,
        must_reset_session: saturation_ratio > HARD_CEILING_RATIO,
        prompt_tokens,
    }
}

pub fn commit_turn_to_ledger(
    session: &mut SessionState,
    eval: &BudgetEvaluation,
    raw_prompt: &str,
    compiled_prompt: &str,
    handoff_summary: Option<String>,
) {
    let compiled_tokens = estimate_tokens(compiled_prompt);
    if eval.must_reset_session {
        session.session_id += 1;
        let handoff = handoff_summary.unwrap_or_default();
        let handoff_tokens = estimate_tokens(&handoff);
        session.accumulated_tokens = handoff_tokens + compiled_tokens;
        session.rolling_summary = handoff;
    } else {
        session.accumulated_tokens += compiled_tokens;
        let snippet: String = raw_prompt
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        if !snippet.is_empty() {
            if !session.rolling_summary.is_empty() {
                session.rolling_summary.push_str(" -> ");
            }
            session.rolling_summary.push_str(&snippet);
            // Cap rolling_summary to last ~600 chars (UTF-8 safe)
            if session.rolling_summary.len() > 600 {
                let mut start = session.rolling_summary.len() - 600;
                while start < session.rolling_summary.len()
                    && !session.rolling_summary.is_char_boundary(start)
                {
                    start += 1;
                }
                session.rolling_summary = session.rolling_summary[start..].to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_reset_strictly_above_35_percent() {
        let state = SessionState {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 35_000,
            rolling_summary: String::new(),
        };
        // 10,000 overhead + 35,000 history + ~100 prompt = 45,100 / 128,000 = 35.23%
        let eval = evaluate_budget(&state, &"a".repeat(360), 10_000, 128_000);
        assert!(eval.must_reset_session);
        assert!(eval.saturation_ratio > 0.35);
    }

    #[test]
    fn stays_in_session_at_or_below_35_percent() {
        let state = SessionState {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 20_000,
            rolling_summary: String::new(),
        };
        // 10,000 overhead + 20,000 history + ~10 prompt = 30,010 / 128,000 = 23.4%
        let eval = evaluate_budget(&state, "fix typo", 10_000, 128_000);
        assert!(!eval.must_reset_session);
    }
}

==> src/compiler.rs <==
use crate::types::{AgentEntry, LlmCompilerOutput, SessionState};
use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

/// Fast heuristic check: bypass LLM rewrite if prompt is a short shell/test command
pub fn is_trivial_passthrough(raw_prompt: &str) -> bool {
    let trimmed = raw_prompt.trim();
    let word_count = trimmed.split_whitespace().count();
    let command_prefixes = ["cargo ", "git ", "npm ", "pytest", "ls", "cd ", "/clear", "/help"];
    word_count <= 4 && command_prefixes.iter().any(|p| trimmed.starts_with(p))
}

pub fn compile_and_route(
    raw_prompt: &str,
    registry: &BTreeMap<String, AgentEntry>,
    session: &SessionState,
    must_reset_session: bool,
) -> LlmCompilerOutput {
    if is_trivial_passthrough(raw_prompt) && !must_reset_session {
        return build_fallback_decision(raw_prompt, registry, session, must_reset_session, true);
    }

    match call_llm_endpoint(raw_prompt, registry, session, must_reset_session) {
        Ok(mut decision) => {
            sanitize_hallucinated_ids(&mut decision, registry);
            decision
        }
        Err(err) => {
            eprintln!("[agent-router] LLM compiler unavailable ({}). Failing open with local heuristic.", err);
            build_fallback_decision(raw_prompt, registry, session, must_reset_session, false)
        }
    }
}

fn call_llm_endpoint(
    raw_prompt: &str,
    registry: &BTreeMap<String, AgentEntry>,
    session: &SessionState,
    must_reset_session: bool,
) -> Result<LlmCompilerOutput, String> {
    let api_url = env::var("ROUTER_LLM_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/v1/chat/completions".to_string());
    let api_key = env::var("ROUTER_LLM_KEY").unwrap_or_default();
    let model = env::var("ROUTER_LLM_MODEL").unwrap_or_else(|_| "qwen2.5-coder:7b".to_string());
    let timeout_ms: u64 = env::var("ROUTER_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);

    let compact_registry: Vec<_> = registry
        .values()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "kind": e.kind,
                "description": e.description,
                "triggers": e.triggers
            })
        })
        .collect();

    let system_prompt = format!(
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
        serde_json::to_string(&compact_registry).unwrap_or_default(),
        session.rolling_summary,
        must_reset_session
    );

    let body = serde_json::json!({
        "model": model,
        "temperature": 0.0,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": raw_prompt }
        ]
    });

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(timeout_ms))
        .timeout_read(Duration::from_millis(timeout_ms))
        .build();

    let mut req = agent.post(&api_url).set("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", api_key));
    }

    let resp: serde_json::Value = req
        .send_json(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;

    let raw_content = resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "Missing choices[0].message.content".to_string())?;

    let cleaned = strip_markdown_fences(raw_content);
    serde_json::from_str::<LlmCompilerOutput>(cleaned).map_err(|e| e.to_string())
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

/// Prevents hallucinated agent or skill names from ever leaving the compiler
pub fn sanitize_hallucinated_ids(
    decision: &mut LlmCompilerOutput,
    registry: &BTreeMap<String, AgentEntry>,
) {
    let valid_agent = registry
        .get(&decision.target_subagent)
        .map(|e| e.kind == "agent")
        .unwrap_or(false);

    if !valid_agent {
        decision.target_subagent = "default".to_string();
    }

    decision.required_skills.retain(|skill_id| {
        registry
            .get(skill_id)
            .map(|e| e.kind == "skill")
            .unwrap_or(false)
    });
}

/// Offline / Fail-Open deterministic compiler & keyword matcher
pub fn build_fallback_decision(
    raw_prompt: &str,
    registry: &BTreeMap<String, AgentEntry>,
    session: &SessionState,
    must_reset_session: bool,
    passthrough: bool,
) -> LlmCompilerOutput {
    let lower_prompt = raw_prompt.to_lowercase();
    let mut matched_agent = "default".to_string();
    let mut matched_skills = Vec::new();

    for entry in registry.values() {
        let hit = entry.triggers.iter().any(|t| lower_prompt.contains(&t.to_lowercase()))
            || lower_prompt.contains(&entry.id.to_lowercase());
        if hit {
            if entry.kind == "agent" && matched_agent == "default" {
                matched_agent = entry.id.clone();
            } else if entry.kind == "skill" {
                matched_skills.push(entry.id.clone());
            }
        }
    }

    let compiled_prompt = if passthrough {
        raw_prompt.to_string()
    } else {
        format!(
            "<objective>\n{}\n</objective>\n<constraints>\n- Follow existing repository patterns and conventions.\n- Do not introduce unrequested dependencies or unrelated refactors.\n</constraints>\n<verification>\n- Run relevant test suites and verify zero compiler/linter errors before completing.\n</verification>",
            raw_prompt.trim()
        )
    };

    let context_handoff_summary = if must_reset_session {
        Some(format!(
            "Prior session reset at >35% context ceiling. Previous summary: {}",
            if session.rolling_summary.is_empty() {
                "None recorded."
            } else {
                &session.rolling_summary
            }
        ))
    } else {
        None
    };

    LlmCompilerOutput {
        passthrough,
        target_subagent: matched_agent,
        required_skills: matched_skills,
        compiled_prompt,
        context_handoff_summary,
    }
}

==> src/dispatcher.rs <==
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

==> src/hook.rs <==
use crate::budget::estimate_tokens;
use crate::run_pipeline_with_context;
use crate::types::RouterEnvelope;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Format the [`RouterEnvelope`] into an imperative context block optimized for VS Code Copilot (`runSubagent`) and Claude Code.
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
        sections.push(format!(
            "<routing_directive>\n{}\n</routing_directive>",
            directives.join("\n")
        ));
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

/// Estimate real session token usage directly from the harness transcript file (`transcript_path` in VS Code / Claude).
fn estimate_transcript_tokens(req: &Value) -> Option<usize> {
    let path_str = req
        .get("transcript_path")
        .or_else(|| req.get("transcriptPath"))
        .and_then(|p| p.as_str())?;

    let content = fs::read_to_string(path_str).ok()?;
    Some(estimate_tokens(&content))
}

/// Extract the host harness's native session ID (`sessionId` in VS Code, `session_id` in Claude/Copilot CLI).
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

pub fn run_stdio_hook(default_repo_root: &Path) -> io::Result<()> {
    let mut raw_input = String::new();
    io::stdin().read_to_string(&mut raw_input)?;

    let mut stdout = io::stdout();
    let trimmed = raw_input.trim();
    if trimmed.is_empty() {
        writeln!(stdout, "{{}}")?;
        stdout.flush()?;
        return Ok(());
    }

    let req: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            writeln!(stdout, "{{}}")?;
            stdout.flush()?;
            return Ok(());
        }
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
                .and_then(|t| t.as_str())
                .and_then(extract_prompt_from_transcript)
        })
        .unwrap_or_default();

    if raw_prompt.trim().is_empty() {
        writeln!(stdout, "{{}}")?;
        stdout.flush()?;
        return Ok(());
    }

    let observed_tokens = estimate_transcript_tokens(&req);
    let host_session_id = extract_host_session_id(&req);

    let response = match run_pipeline_with_context(
        &repo_root,
        &raw_prompt,
        observed_tokens,
        host_session_id,
    ) {
        Ok(envelope) => {
            let context = format_hook_context(&envelope);
            if is_agy_hook(&req) {
                let agy_msg = if envelope.session_meta.threshold_exceeded {
                    format!(
                        "<circuit_breaker_alert>\nSTOP: Context saturation reached {:.1}% (>35% hard ceiling). State handoff saved to `.router/HANDOFF.md`. Start a new conversation.\n</circuit_breaker_alert>\n\n{}",
                        envelope.session_meta.saturation_ratio * 100.0,
                        context
                    )
                } else {
                    context
                };
                json!({
                    "injectSteps": [
                        {
                            "ephemeralMessage": agy_msg
                        }
                    ]
                })
            } else if envelope.session_meta.threshold_exceeded {
                let event_name = req
                    .get("hookEventName")
                    .or_else(|| req.get("hook_event_name"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("UserPromptSubmit");
                let stop_msg = format!(
                    "⚡ AgentRouter Circuit Breaker: Context reached {:.1}% (>35% ceiling). State handoff & compiled prompt saved to .router/HANDOFF.md. Click '+' (New Chat) or run `/clear` and re-submit your prompt.",
                    envelope.session_meta.saturation_ratio * 100.0
                );
                // Compatible with BOTH VS Code Copilot (`continue: false`, `stopReason`) and Claude Code (`decision: "block"`, `reason`)
                json!({
                    "continue": false,
                    "stopReason": stop_msg,
                    "systemMessage": stop_msg,
                    "decision": "block",
                    "reason": stop_msg,
                    "hookSpecificOutput": {
                        "hookEventName": event_name,
                        "additionalContext": context
                    }
                })
            } else {
                let event_name = req
                    .get("hookEventName")
                    .or_else(|| req.get("hook_event_name"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("UserPromptSubmit");
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
                        "hookEventName": event_name,
                        "additionalContext": context
                    }
                })
            }
        }
        Err(e) => {
            eprintln!("[agent-router] Hook pipeline error (failing open): {}", e);
            json!({})
        }
    };

    writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
    stdout.flush()?;
    Ok(())
}

==> src/main.rs <==
mod budget;
mod compiler;
mod dispatcher;
mod hook;
mod registry;
mod types;

use std::env;
use std::fs;
use std::path::Path;
use types::RouterEnvelope;

pub fn run_pipeline(repo_root: &Path, raw_prompt: &str) -> Result<RouterEnvelope, String> {
    run_pipeline_with_context(repo_root, raw_prompt, None, None)
}

pub fn run_pipeline_with_context(
    repo_root: &Path,
    raw_prompt: &str,
    observed_transcript_tokens: Option<usize>,
    host_session_id: Option<String>,
) -> Result<RouterEnvelope, String> {
    let router_dir = repo_root.join(".router");
    fs::create_dir_all(&router_dir).map_err(|e| e.to_string())?;

    let base_overhead: usize = env::var("ROUTER_BASE_OVERHEAD_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let max_window: usize = env::var("ROUTER_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128_000);

    let session_file = router_dir.join("session.json");
    let handoff_file = router_dir.join("HANDOFF.md");
    let mut session = budget::load_session(&session_file);

    // If the host harness started a brand new session (e.g. after `/clear` or clicking `+`),
    // reset token counter while keeping any pending handoff intact for the first turn.
    if let Some(ref new_host_id) = host_session_id {
        if session.host_session_id.as_ref() != Some(new_host_id) {
            if session.host_session_id.is_some() || handoff_file.exists() {
                session.accumulated_tokens = 0;
            }
            session.host_session_id = Some(new_host_id.clone());
        }
    }

    // If the hook provided a live transcript path, sync accumulated_tokens with real transcript size
    if let Some(real_tokens) = observed_transcript_tokens {
        if real_tokens > session.accumulated_tokens {
            session.accumulated_tokens = real_tokens;
        }
    }

    // Step 0: Evaluate Context Budget (35% Ceiling)
    let eval = budget::evaluate_budget(&session, raw_prompt, base_overhead, max_window);

    // Step 2: Load or Build XXH3 Mtime-Cached Capability Registry
    let cache_file = router_dir.join("registry.json");
    let registry = registry::load_or_build_registry(repo_root, &cache_file);

    // Step 1: Compile Prompt & Select Subagent/Skills (Fail-Open Resilient)
    let mut decision =
        compiler::compile_and_route(raw_prompt, &registry, &session, eval.must_reset_session);

    // Persist or consume `.router/HANDOFF.md` across blocked -> fresh session transitions
    if eval.must_reset_session {
        if !handoff_file.exists() {
            if let Some(ref handoff) = decision.context_handoff_summary {
                let _ = fs::write(&handoff_file, handoff);
            }
        }
    } else if handoff_file.exists() && !decision.passthrough {
        if let Ok(pending_handoff) = fs::read_to_string(&handoff_file) {
            let trimmed = pending_handoff.trim();
            if !trimmed.is_empty() && decision.context_handoff_summary.is_none() {
                decision.context_handoff_summary = Some(trimmed.to_string());
            }
        }
        let _ = fs::remove_file(&handoff_file);
    }

    // Commit Updated Token Ledger
    let prior_tokens = session.accumulated_tokens;
    budget::commit_turn_to_ledger(
        &mut session,
        &eval,
        raw_prompt,
        &decision.compiled_prompt,
        decision.context_handoff_summary.clone(),
    );
    // If we are in Hook mode (`host_session_id.is_some()`) and blocked the turn (`must_reset_session`),
    // keep `accumulated_tokens` pinned until `host_session_id` actually changes on New Chat (`+`)!
    if eval.must_reset_session && host_session_id.is_some() {
        session.accumulated_tokens = prior_tokens;
    }
    budget::save_session(&session_file, &session).map_err(|e| e.to_string())?;

    eprintln!(
        "[agent-router] prompt_tokens={} projected_saturation={:.3} reset={}",
        eval.prompt_tokens, eval.saturation_ratio, eval.must_reset_session
    );

    // Step 3: Build Harness-Agnostic Envelope
    Ok(dispatcher::build_envelope(&session, &eval, decision, &registry))
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let repo_root = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());

    if args.is_empty() {
        eprintln!("Usage:\n  agent-router \"<raw_prompt>\"\n  agent-router --hook\n  agent-router --sync-tokens <exact_token_count>");
        std::process::exit(1);
    }

    match args[0].as_str() {
        "--hook" => {
            if let Err(e) = hook::run_stdio_hook(&repo_root) {
                eprintln!("[agent-router] Hook error: {}", e);
                std::process::exit(1);
            }
        }
        "--sync-tokens" => {
            match args.get(1).and_then(|s| s.parse::<usize>().ok()) {
                Some(val) => {
                    // The ledger may not exist yet (first calibration before any routed
                    // turn), so ensure the state directory is present before writing.
                    let router_dir = repo_root.join(".router");
                    if let Err(e) = fs::create_dir_all(&router_dir) {
                        eprintln!("[agent-router] cannot create {}: {}", router_dir.display(), e);
                        std::process::exit(1);
                    }
                    let session_file = router_dir.join("session.json");
                    let mut session = budget::load_session(&session_file);
                    session.accumulated_tokens = val;
                    if let Err(e) = budget::save_session(&session_file, &session) {
                        eprintln!("[agent-router] failed to persist token ledger: {}", e);
                        std::process::exit(1);
                    }
                    eprintln!("[agent-router] ledger calibrated to {} tokens", val);
                }
                None => {
                    eprintln!("Usage: agent-router --sync-tokens <exact_token_count>");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            let raw_prompt = args.join(" ");
            match run_pipeline(&repo_root, &raw_prompt) {
                Ok(envelope) => match dispatcher::execute_dispatch(&envelope) {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("[agent-router] Dispatch error: {}", e);
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("[agent-router] Pipeline error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

==> src/registry.rs <==
use crate::types::{AgentEntry, RegistryCache};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;
use xxhash_rust::xxh3::xxh3_64;

const SEARCH_DIRS: &[(&str, &str)] = &[
    (".agents", "agent"),
    (".claude/agents", "agent"),
    (".github/agents", "agent"),
    ("skills", "skill"),
    (".claude/skills", "skill"),
    (".github/skills", "skill"),
    (".github/prompts", "skill"),
    (".github/instructions", "skill"),
];

pub fn load_or_build_registry(
    repo_root: &Path,
    cache_file: &Path,
) -> BTreeMap<String, AgentEntry> {
    let mut fingerprint = String::new();
    let mut discovered_files: Vec<(PathBuf, &'static str)> = Vec::new();

    for (rel_dir, kind) in SEARCH_DIRS {
        let full_dir = repo_root.join(rel_dir);
        if !full_dir.exists() {
            continue;
        }
        let mut paths: Vec<PathBuf> = WalkDir::new(&full_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        paths.sort();

        for path in paths {
            if let Ok(meta) = fs::metadata(&path) {
                let nanos = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                fingerprint.push_str(&format!("{}:{}|", path.display(), nanos));
                discovered_files.push((path, *kind));
            }
        }
    }

    let current_hash = xxh3_64(fingerprint.as_bytes());

    if let Ok(raw) = fs::read_to_string(cache_file) {
        if let Ok(cached) = serde_json::from_str::<RegistryCache>(&raw) {
            if cached.hash == current_hash {
                return cached.entries;
            }
        }
    }

    let mut entries = BTreeMap::new();
    for (file_path, kind) in discovered_files {
        if let Ok(content) = fs::read_to_string(&file_path) {
            let entry = parse_manifest(repo_root, &file_path, kind, &content);
            entries.insert(entry.id.clone(), entry);
        }
    }

    let cache = RegistryCache {
        hash: current_hash,
        entries: entries.clone(),
    };
    let _ = fs::write(cache_file, serde_json::to_string_pretty(&cache).unwrap_or_default());
    entries
}

/// Zero-dependency parser for YAML frontmatter (`--- ... ---`) with Markdown header fallback
pub fn parse_manifest(
    repo_root: &Path,
    file_path: &Path,
    kind: &str,
    content: &str,
) -> AgentEntry {
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
                    "triggers" => {
                        let cleaned = val.trim_start_matches('[').trim_end_matches(']');
                        triggers = cleaned
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
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

    AgentEntry {
        id,
        kind: kind.to_string(),
        path: rel_path,
        description,
        triggers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_frontmatter_accurately() {
        let md = "---\nname: sec-auditor\ndescription: Audits Rust unsafe blocks\ntriggers: [security, unsafe, audit]\n---\n# Body";
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/.agents/sec.md"), "agent", md);
        assert_eq!(entry.id, "sec-auditor");
        assert_eq!(entry.description, "Audits Rust unsafe blocks");
        assert_eq!(entry.triggers, vec!["security", "unsafe", "audit"]);
    }

    #[test]
    fn strips_agent_and_instructions_double_extension_when_name_omitted() {
        let md = "---\ndescription: Custom VS Code agent without explicit name\ntriggers: [rust]\n---\n# Body";
        let entry = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/agents/rust-architect.agent.md"),
            "agent",
            md,
        );
        assert_eq!(entry.id, "rust-architect");

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
    }

    #[test]
    fn truncates_long_multibyte_description_without_panicking() {
        // 2-byte chars make byte index 240 land mid-char; must snap back to a boundary.
        let md = format!("# Title\n{}", "é".repeat(300));
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/no-front.md"), "agent", &md);
        assert!(entry.description.len() <= 240);
        assert!(entry.description.is_char_boundary(entry.description.len()));
    }
}

==> src/types.rs <==
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
    #[serde(default)]
    pub host_session_id: Option<String>,
    pub accumulated_tokens: usize,
    pub rolling_summary: String,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: 1,
            host_session_id: None,
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

/// Final deterministic payload emitted to stdout or formatted for lifecycle hooks
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

==> tests/hooks.rs <==
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

==> tests/pipeline.rs <==
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

==> tests/common/mod.rs <==
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

````
