mod budget;
mod compiler;
mod dispatcher;
mod mcp;
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

    // Step 3: Build Harness-Agnostic Envelope
    Ok(dispatcher::build_envelope(&session, &eval, decision, &registry))
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let repo_root = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());

    if args.is_empty() {
        eprintln!("Usage:\n  agent-router \"<raw_prompt>\"\n  agent-router --mcp\n  agent-router --sync-tokens <exact_token_count>");
        std::process::exit(1);
    }

    match args[0].as_str() {
        "--mcp" => {
            if let Err(e) = mcp::run_mcp_stdio_loop(&repo_root) {
                eprintln!("[agent-router] MCP loop error: {}", e);
                std::process::exit(1);
            }
        }
        "--sync-tokens" => {
            if let Some(val) = args.get(1).and_then(|s| s.parse::<usize>().ok()) {
                let session_file = repo_root.join(".router/session.json");
                let mut session = budget::load_session(&session_file);
                session.accumulated_tokens = val;
                let _ = budget::save_session(&session_file, &session);
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
