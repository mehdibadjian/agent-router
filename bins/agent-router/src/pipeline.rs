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
