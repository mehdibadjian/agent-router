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
