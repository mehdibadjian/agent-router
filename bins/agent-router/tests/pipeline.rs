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
