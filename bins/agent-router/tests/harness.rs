//! Spec §5 harness tests for the thin binary: `--prompt`/stdin, `--mcp-stdio`,
//! `--wrap <CMD>`, and the legacy `--hook`/`--sync-tokens`/positional surface.
//!
//! These are compatibility tests: the v2 binary must keep the v0.3.0 drop-in behaviour
//! (JSON envelopes, `ROUTED_*` env block, exit-code propagation) while adding the v2
//! flags on top of it.

mod common;

use common::fixture_repo;
use common::run;
use serde_json::Value;

const OFFLINE: &[(&str, &str)] = &[
    ("ROUTER_LLM_URL", "http://127.0.0.1:1/v1/chat/completions"),
    ("ROUTER_TIMEOUT_MS", "150"),
];

fn read_json(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read state file"))
        .expect("parse state")
}

/// One newline-delimited JSON-RPC frame per line.
fn rpc(id: u64, method: &str, params: Value) -> String {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("serialize frame")
}

fn lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad frame {l}: {e}")))
        .collect()
}

#[test]
fn explicit_prompt_flag_emits_the_legacy_envelope() {
    let repo = fixture_repo("cli-prompt");
    let (stdout, _stderr, code) = run(
        &repo,
        &["--prompt", "fix rust lifetime in parser and add test"],
        OFFLINE,
        None,
    );
    assert_eq!(code, 0);
    let envelope: Value = serde_json::from_str(&stdout).expect("pure JSON envelope");
    assert_eq!(envelope["routing"]["target_subagent"], "rust-architect");
    assert_eq!(envelope["session_meta"]["action"], "CONTINUE_SESSION");
    assert!(envelope["payload"]["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("<objective>"));

    // The ledger is committed exactly like the legacy binary did.
    let state = read_json(&format!("{}/.router/session.json", repo));
    assert_eq!(state["session_id"].as_u64().unwrap(), 1);
    assert!(state["accumulated_tokens"].as_u64().unwrap() > 0);
    assert!(state["rolling_summary"]
        .as_str()
        .unwrap()
        .contains("fix rust lifetime"));
}

#[test]
fn prompt_flag_without_value_reads_stdin() {
    let repo = fixture_repo("cli-stdin");
    let (stdout, _stderr, code) = run(&repo, &["--prompt"], OFFLINE, Some("adopt tdd workflow\n"));
    assert_eq!(code, 0, "stdin prompt must route");
    let envelope: Value = serde_json::from_str(&stdout).expect("envelope");
    assert_eq!(envelope["routing"]["passthrough"], false);
    assert!(envelope["payload"]["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("adopt tdd workflow"));
}

#[test]
fn wrap_dispatches_to_the_child_with_the_routed_env_block() {
    let repo = fixture_repo("cli-wrap");
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

    let (stdout, _stderr, code) = run(
        &repo,
        &["--wrap", harness.as_str(), "fix", "rust", "lifetime", "bug"],
        OFFLINE,
        None,
    );
    assert_eq!(code, 7, "wrap must propagate the child exit code");
    assert!(stdout.is_empty(), "wrap mode must not print the envelope: {stdout}");

    let marker = std::fs::read_to_string(format!("{}/marker.txt", repo)).expect("marker");
    let mut parts = marker.trim().split('|');
    assert_eq!(parts.next(), Some("rust-architect"));
    assert_eq!(parts.next(), Some("CONTINUE_SESSION"));
    let prompt = parts.nth(1).unwrap_or("");
    assert!(prompt.contains("<objective>"), "child got the compiled prompt");
    assert!(prompt.contains("fix rust lifetime bug"), "child prompt: {prompt}");
}

#[test]
fn mcp_stdio_serves_the_three_router_tools() {
    let repo = fixture_repo("mcp-basic");
    let frames = vec![
        rpc(1, "initialize", serde_json::json!({"protocolVersion": "2024-11-05"})),
        rpc(2, "tools/list", serde_json::json!({})),
    ];
    let (stdout, stderr, code) = run(
        &repo,
        &["--mcp-stdio"],
        OFFLINE,
        Some(&format!("{}\n", frames.join("\n"))),
    );
    assert_eq!(code, 0, "mcp server must exit cleanly: {stderr}");

    let replies = lines(&stdout);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "agent-router");

    let tools = replies[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"route_intent")
            && names.contains(&"check_context_budget")
            && names.contains(&"refresh_registry"),
        "expected the three router tools, got {names:?}"
    );
    assert_eq!(tools[0]["inputSchema"]["type"], "object");
}

#[test]
fn mcp_route_intent_returns_the_envelope_and_persists_state() {
    let repo = fixture_repo("mcp-route");
    let frames = vec![
        rpc(
            1,
            "tools/call",
            serde_json::json!({
                "name": "route_intent",
                "arguments": {"raw_prompt": "fix rust lifetime in parser", "force_compile": false}
            }),
        ),
        // A notification must never be answered.
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}".to_string(),
        rpc(
            2,
            "tools/call",
            serde_json::json!({
                "name": "check_context_budget",
                "arguments": {"raw_prompt": "implement a trie", "context_tokens_used": 75000}
            }),
        ),
    ];
    let (stdout, _stderr, code) = run(
        &repo,
        &["--mcp-stdio"],
        OFFLINE,
        Some(&format!("{}\n", frames.join("\n"))),
    );
    assert_eq!(code, 0);

    let replies = lines(&stdout);
    assert_eq!(replies.len(), 2, "notifications get no reply");

    let routed: Value = serde_json::from_str(
        replies[0]["result"]["content"][0]["text"]
            .as_str()
            .expect("route text"),
    )
    .expect("route envelope json");
    assert_eq!(routed["routing"]["target_subagent"], "rust-architect");
    assert_eq!(routed["session_meta"]["action"], "CONTINUE_SESSION");
    assert_eq!(replies[0]["result"]["isError"], false);

    let budget: Value = serde_json::from_str(
        replies[1]["result"]["content"][0]["text"]
            .as_str()
            .expect("budget text"),
    )
    .expect("budget json");
    assert!(budget["circuit_tripped"].as_bool().unwrap(), "{budget}");
    assert_eq!(budget["hard_ceiling_ratio"], serde_json::json!(0.35));
    assert!(budget["saturation_ratio"].as_f64().unwrap() > 0.35);

    // route_intent committed a turn to the same ledger the CLI uses.
    let state = read_json(&format!("{}/.router/session.json", repo));
    assert!(state["accumulated_tokens"].as_u64().unwrap() > 0);
}

#[test]
fn mcp_errors_and_refresh_registry_are_well_formed() {
    let repo = fixture_repo("mcp-errors");
    // Seed the cache so refresh has something to drop.
    run(&repo, &["probe"], OFFLINE, None);
    let cache = format!("{}/.router/registry.json", repo);
    assert!(std::path::Path::new(&cache).exists());
    std::fs::write(&cache, "not json at all").expect("clobber cache");

    let frames = vec![
        rpc(1, "tools/call", serde_json::json!({"name": "nope", "arguments": {}})),
        rpc(2, "tools/call", serde_json::json!({"name": "route_intent", "arguments": {}})),
        rpc(3, "tools/call", serde_json::json!({"name": "refresh_registry", "arguments": {}})),
        rpc(4, "bogus/method", serde_json::json!({})),
        "{ broken json".to_string(),
    ];
    let (stdout, _stderr, code) = run(
        &repo,
        &["--mcp-stdio"],
        OFFLINE,
        Some(&format!("{}\n", frames.join("\n"))),
    );
    assert_eq!(code, 0);

    let replies = lines(&stdout);
    assert_eq!(replies.len(), 5);
    assert_eq!(replies[0]["error"]["code"], -32602, "unknown tool");
    // A missing required argument is the tool's failure, not the protocol's (MCP semantics).
    assert_eq!(replies[1]["result"]["isError"], true, "missing raw_prompt");
    assert!(
        replies[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("raw_prompt"),
        "{}",
        replies[1]
    );
    assert_eq!(replies[2]["result"]["isError"], false, "refresh ok");
    assert_eq!(replies[3]["error"]["code"], -32601, "unknown method");
    assert_eq!(replies[4]["error"]["code"], -32700, "parse error");

    // refresh_registry rebuilt the snapshot from the manifests.
    let entries = read_json(&cache)["entries"]
        .as_object()
        .cloned()
        .expect("entries map");
    assert!(entries.contains_key("rust-architect"), "{entries:?}");
    assert!(entries.contains_key("tdd-workflow"), "{entries:?}");
}

#[test]
fn unknown_flag_still_routes_as_a_legacy_positional_prompt() {
    let repo = fixture_repo("cli-positional");
    let (stdout, _stderr, code) = run(&repo, &["fix", "rust", "lifetime"], OFFLINE, None);
    assert_eq!(code, 0);
    let envelope: Value = serde_json::from_str(&stdout).expect("envelope");
    assert_eq!(envelope["routing"]["target_subagent"], "rust-architect");
    assert!(envelope["payload"]["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("fix rust lifetime"));
}

#[test]
fn no_arguments_prints_usage_and_exits_nonzero() {
    let repo = fixture_repo("cli-usage");
    let (stdout, stderr, code) = run(&repo, &[], OFFLINE, None);
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    for flag in ["--hook", "--sync-tokens", "--prompt", "--mcp-stdio", "--wrap"] {
        assert!(stderr.contains(flag), "usage must document {flag}: {stderr}");
    }
}
