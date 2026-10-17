use crate::budget::estimate_tokens;
use crate::run_pipeline_with_context;
use crate::types::RouterEnvelope;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Format the [`RouterEnvelope`] into an imperative context block that forces subagent & skill usage.
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

    // Imperative routing directive so the host harness actually delegates to the subagent & skills
    if envelope.routing.target_subagent != "default" || !envelope.routing.required_skills.is_empty() {
        let mut directives = Vec::new();
        if envelope.routing.target_subagent != "default" {
            directives.push(format!(
                "- MANDATORY SUBAGENT: Delegate execution to subagent `{}` (defined at `{}`).",
                envelope.routing.target_subagent, envelope.routing.target_subagent_path
            ));
        }
        if !envelope.routing.required_skills.is_empty() {
            directives.push(format!(
                "- MANDATORY SKILLS: Load and follow the skill instructions at: `{}`.",
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
        // Use `let Ok(...) else { continue }` so a single malformed line doesn't abort the scan
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

/// Estimate real session token usage directly from the harness transcript file if provided.
fn estimate_transcript_tokens(req: &Value) -> Option<usize> {
    let path_str = req
        .get("transcript_path")
        .or_else(|| req.get("transcriptPath"))
        .and_then(|p| p.as_str())?;

    let content = fs::read_to_string(path_str).ok()?;
    Some(estimate_tokens(&content))
}

/// Extract the host harness's native session/conversation ID to detect `/clear` or new sessions.
fn extract_host_session_id(req: &Value) -> Option<String> {
    req.get("session_id")
        .or_else(|| req.get("sessionId"))
        .or_else(|| req.get("conversationId"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// Detect whether the hook input came from Antigravity (`PreInvocation` / `SessionStart`)
/// vs. VS Code Copilot / Claude Code (`UserPromptSubmit`).
fn is_agy_hook(input: &Value) -> bool {
    input.get("invocationNum").is_some()
        || input.get("workspacePaths").is_some()
        || input.get("transcriptPath").is_some()
        || input.get("conversationId").is_some()
}

/// Execute hook mode: read a JSON payload from stdin, run the router pipeline,
/// and emit the harness-appropriate hook response JSON to stdout.
///
/// Supports:
/// - VS Code GitHub Copilot & Claude Code (`UserPromptSubmit` -> `hookSpecificOutput.additionalContext`)
/// - Antigravity / AGY (`PreInvocation` -> `injectSteps[].ephemeralMessage`)
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
            // Never block the host harness on malformed hook input (fail-open).
            writeln!(stdout, "{{}}")?;
            stdout.flush()?;
            return Ok(());
        }
    };

    // Resolve workspace root from payload (`cwd` for Copilot/Claude, `workspacePaths[0]` for AGY)
    let repo_root: PathBuf = req
        .get("cwd")
        .and_then(|c| c.as_str())
        .or_else(|| req.pointer("/workspacePaths/0").and_then(|p| p.as_str()))
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(|| default_repo_root.to_path_buf());

    // Extract prompt from `prompt` field or fallback to AGY `transcriptPath`
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
                json!({
                    "injectSteps": [
                        {
                            "ephemeralMessage": context
                        }
                    ]
                })
            } else if envelope.session_meta.threshold_exceeded {
                let event_name = req
                    .get("hook_event_name")
                    .or_else(|| req.get("hookEventName"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("UserPromptSubmit");
                // Hard Circuit Breaker: Block the bloated session turn and surface the handoff
                json!({
                    "decision": "block",
                    "reason": format!(
                        "Context saturation reached {:.1}% (>35% hard ceiling). Session handoff & compiled prompt have been saved to .router/HANDOFF.md and .router/session.json. Please run `/clear` (or start a new session) and re-run your prompt.",
                        envelope.session_meta.saturation_ratio * 100.0
                    ),
                    "hookSpecificOutput": {
                        "hookEventName": event_name,
                        "additionalContext": context
                    }
                })
            } else {
                let event_name = req
                    .get("hook_event_name")
                    .or_else(|| req.get("hookEventName"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("UserPromptSubmit");
                json!({
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
