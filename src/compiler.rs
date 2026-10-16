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
