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
    } else if handoff_file.exists() {
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
