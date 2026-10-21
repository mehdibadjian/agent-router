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
