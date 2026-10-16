# Zero-Hallucination Implementation Plan

**Project:** AgentRouter (Zero-Dependency System 1 Pre-Processor & Subagent Dispatcher)
**Document Version:** 1.0
**Date:** October 16, 2026
**Companion Document:** [BRD.md](./BRD.md)

This Zero-Hallucination Implementation Plan is structured as an execution-ready blueprint you can hand directly to a coding agent (or follow file-by-file). It locks down exact crate versions, prohibits unrequested dependencies (no tokio, no openssl, no clap, no serde_yaml), defines every struct and function signature, and includes deterministic unit tests for every module.

## 1. Anti-Hallucination Guardrails (Agent Constraints)

Any coding agent executing this specification MUST obey these five hard rules:

* **Strict Crate Lock:** Only the 5 crates in `Cargo.toml` are permitted (`serde`, `serde_json`, `ureq`, `xxhash-rust`, `walkdir`). Do not add `clap`, `tokio`, `reqwest`, `anyhow`, `regex`, or `serde_yaml`.
* **Synchronous I/O Only:** All HTTP, filesystem, and stdin/stdout hook JSON operations must use blocking standard library I/O (`std::io`, `std::fs`, `ureq`).
* **Pure-Rust TLS:** `ureq` v2.10 must be configured with `default-features = false` and `features = ["tls", "json"]` so it compiles `rustls` statically without linking system OpenSSL.
* **Stdout Purity:** Never use `println!` for logging or diagnostics—stdout is strictly reserved for the JSON routing contract and lifecycle hook responses. All warnings/logs must use `eprintln!`.
* **Fail-Open Mandate:** Network timeouts, malformed LLM JSON, or missing API keys must never panic or crash the binary; they must fall back deterministically to `FallbackDecision`.

## 2. Repository Directory Layout

```
agent-router/
├── .cargo/
│   └── config.toml        # Static linking flags for Linux musl & Windows MSVC
├── Cargo.toml             # Locked dependencies & size-optimized release profile
└── src/
    ├── main.rs            # CLI parser (std::env::args) & orchestration pipeline
    ├── types.rs           # Shared data contracts & serde structs
    ├── budget.rs          # Step 0: Token estimator & 35% circuit breaker
    ├── registry.rs        # Step 2: XXH3 mtime scanner & frontmatter parser
    ├── compiler.rs        # Step 1: Low-cost LLM client, prompt compiler & fail-open fallback
    ├── dispatcher.rs      # Step 3: CLI subprocess spawner & stdout JSON emitter
    └── hook.rs            # Step 3 (Mode C): Synchronous lifecycle hook adapter (--hook)
```

## 3. Configuration & Build Manifests

### File 1: `.cargo/config.toml`

```toml
[target.x86_64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]

[target.aarch64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]

[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```

### File 2: `Cargo.toml`

```toml
[package]
name = "agent-router"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1.0.210", features = ["derive"] }
serde_json = "1.0.128"
ureq = { version = "2.10.1", default-features = false, features = ["tls", "json"] }
xxhash-rust = { version = "0.8.12", features = ["xxh3"] }
walkdir = "2.5.0"

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

## 4. Module-by-Module Code Specifications

### File 3: `src/types.rs` (Canonical Data Contracts)

Defines every state file and output payload so no module invents ad-hoc JSON keys.

```rust
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
```

### File 4: `src/budget.rs` (Step 0: Token Estimator & 35% Circuit Breaker)

Implements zero-dependency BPE token estimation, ledger persistence, and the 35% hard ceiling.

```rust
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_reset_strictly_above_35_percent() {
        let state = SessionState {
            session_id: 1,
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
            accumulated_tokens: 20_000,
            rolling_summary: String::new(),
        };
        // 10,000 overhead + 20,000 history + ~10 prompt = 30,010 / 128,000 = 23.4%
        let eval = evaluate_budget(&state, "fix typo", 10_000, 128_000);
        assert!(!eval.must_reset_session);
    }
}
```

### File 5: `src/registry.rs` (Step 2: Mtime-Cached Skill & Agent Scanner)

Includes a zero-dependency YAML frontmatter extractor so the router reads `name`, `description`, and `triggers` without pulling in `serde_yaml`.

```rust
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
        file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-agent")
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
    fn truncates_long_multibyte_description_without_panicking() {
        // 2-byte chars make byte index 240 land mid-char; must snap back to a boundary.
        let md = format!("# Title\n{}", "é".repeat(300));
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/no-front.md"), "agent", &md);
        assert!(entry.description.len() <= 240);
        assert!(entry.description.is_char_boundary(entry.description.len()));
    }
}
```

### File 6: `src/compiler.rs` (Step 1: Prompt Compiler & Fail-Open Fallback)

Implements the complexity passthrough check, the deterministic prompt-engineering system prompt, Hallucination Guard validation (stripping any invented agent/skill IDs), and the offline keyword fallback.

```rust
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
```

### File 7: `src/dispatcher.rs` (Step 3: Harness-Agnostic Envelope & CLI Spawner)

```rust
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
```

### File 8: `src/hook.rs` (Step 3 Mode C: Zero-Dependency Lifecycle Hook Adapter)

Implements synchronous stdin/stdout lifecycle hook handling (`agent-router --hook`) for VS Code GitHub Copilot, Claude Code, and Antigravity (AGY).

```rust
use crate::run_pipeline;
use crate::types::RouterEnvelope;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Format the [`RouterEnvelope`] into a concise context block injected by hooks.
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

    if let Some(handoff) = &envelope.payload.context_handoff_summary {
        if envelope.session_meta.threshold_exceeded {
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
        let entry: Value = serde_json::from_str(trimmed).ok()?;
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

    let response = match run_pipeline(&repo_root, &raw_prompt) {
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
```

### File 9: `src/main.rs` (CLI Entrypoint & Pipeline Orchestrator)

```rust
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

    // Step 0: Evaluate Context Budget (35% Ceiling)
    let session_file = router_dir.join("session.json");
    let mut session = budget::load_session(&session_file);
    let eval = budget::evaluate_budget(&session, raw_prompt, base_overhead, max_window);

    // Step 2: Load or Build XXH3 Mtime-Cached Capability Registry
    let cache_file = router_dir.join("registry.json");
    let registry = registry::load_or_build_registry(repo_root, &cache_file);

    // Step 1: Compile Prompt & Select Subagent/Skills (Fail-Open Resilient)
    let decision = compiler::compile_and_route(raw_prompt, &registry, &session, eval.must_reset_session);

    // Commit Updated Token Ledger
    budget::commit_turn_to_ledger(
        &mut session,
        &eval,
        &decision.compiled_prompt,
        decision.context_handoff_summary.clone(),
    );
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
```

## 5. Step-by-Step Build & Verification Sequence

### 1. Scaffold and Verify Unit Tests

> Zero network calls required

Create the files above and run the built-in test suite to verify the 35% context ceiling math and frontmatter parser:

```bash
cargo test
```

### 2. Verify Fail-Open Offline Execution

> Tests local fallback when no LLM is running

Create a sample agent in `.agents/rust-architect.md`, then run the binary with an unreachable port to verify fail-open behavior:

```bash
mkdir -p .agents skills/tdd
printf -- "---\nname: rust-architect\ndescription: Systems architect\ntriggers: [rust, borrow, lifetime]\n---\n" > .agents/rust-architect.md
printf -- "---\nname: tdd-workflow\ndescription: Red-Green-Refactor\ntriggers: [test, tdd]\n---\n" > skills/tdd/SKILL.md
ROUTER_LLM_URL="http://127.0.0.1:59999/v1/chat/completions" ROUTER_TIMEOUT_MS=200 cargo run -- "fix rust lifetime in parser and add test"
```

Verify that stdout outputs valid JSON selecting `rust-architect` and `skills/tdd/SKILL.md` in under 250ms.

### 3. Verify 35% Hard Reset Trigger

> Sync the ledger near the threshold (e.g., 36,000 tokens + 10,000 base overhead = 46,000 / 128,000 = 35.9%) and run a prompt:

```bash
cargo run -- --sync-tokens 36000
cargo run -- "implement next module"
```

Confirm `session_meta.threshold_exceeded` is `true`, `session_meta.action` is `"START_NEW_SESSION"`, and `session_meta.session_id` increments to `2`.

### 4. Verify Lifecycle Hook Execution (`--hook`)

> Pipe a `UserPromptSubmit` hook payload directly to `--hook` mode:

```bash
printf '{"hook_event_name":"UserPromptSubmit","prompt":"fix rust lifetime in parser and add test"}\n' | cargo run -- --hook
```

Confirm a single-line JSON response containing `hookSpecificOutput.additionalContext` with `<router_meta ... />` and `<objective>`.
