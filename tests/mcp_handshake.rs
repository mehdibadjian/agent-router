//! Integration tests for the synchronous JSON-RPC 2.0 MCP stdio server.
//! Mirrors the plan's `cargo test --test mcp_handshake` gate.

mod common;

use common::fixture_repo;
use common::run;
use serde_json::Value;

/// Send a batch of JSON-RPC lines and collect one parsed response per line.
fn rpc(repo: &str, requests: &[Value]) -> Vec<Value> {
    let mut input = String::new();
    for r in requests {
        input.push_str(&r.to_string());
        input.push('\n');
    }
    let (stdout, _stderr, code) = run(repo, &["--mcp"], &[], Some(&input));
    assert_eq!(code, 0, "mcp loop should exit cleanly on stdin EOF");
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("non-JSON stdout line: {e}\n{l}")))
        .collect()
}

#[test]
fn initialize_reports_protocol_version_and_server_info() {
    let repo = fixture_repo("mcp-init");
    let resp = rpc(&repo, &[serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})]);
    assert_eq!(resp.len(), 1);
    assert_eq!(resp[0]["jsonrpc"], "2.0");
    assert_eq!(resp[0]["id"], 1);
    assert_eq!(resp[0]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp[0]["result"]["serverInfo"]["name"], "agent-router");
    assert!(resp[0]["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn tools_list_exposes_route_and_compile_with_input_schema() {
    let repo = fixture_repo("mcp-list");
    let resp = rpc(&repo, &[serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}})]);
    let tools = resp[0]["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "route_and_compile");
    assert_eq!(tools[0]["inputSchema"]["required"][0], "prompt");
    assert!(tools[0]["description"]
        .as_str()
        .unwrap()
        .contains("35% context ceiling"));
}

#[test]
fn tools_call_returns_envelope_json_inside_text_content() {
    let repo = fixture_repo("mcp-call");
    // Fail open (no LLM reachable) so the call is deterministic and offline.
    let resp = rpc(
        &repo,
        &[serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"route_and_compile","arguments":{"prompt":"fix rust lifetime"}}
        })],
    );
    // The router's own default LLM URL is unreachable in CI, so isError must be false
    // because fail-open is a success path, not an error.
    assert_eq!(resp[0]["result"]["isError"], false);
    let text = resp[0]["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let envelope: Value = serde_json::from_str(text).expect("envelope JSON in tool result");
    assert_eq!(envelope["routing"]["target_subagent"], "rust-architect");
    assert_eq!(envelope["session_meta"]["action"], "CONTINUE_SESSION");
    assert_eq!(resp[0]["result"]["content"][0]["type"], "text");
}

#[test]
fn notifications_are_ignored_without_a_response() {
    let repo = fixture_repo("mcp-notif");
    // One notification (no id) + one real request. Only the request may produce output.
    let resp = rpc(
        &repo,
        &[
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/list","params":{}}),
        ],
    );
    assert_eq!(resp.len(), 1, "notifications must not emit a response");
    assert_eq!(resp[0]["id"], 9);
}

#[test]
fn unknown_method_yields_jsonrpc_error_32601() {
    let repo = fixture_repo("mcp-unknown");
    let resp = rpc(&repo, &[serde_json::json!({"jsonrpc":"2.0","id":5,"method":"nope"})]);
    assert_eq!(resp[0]["error"]["code"], -32601);
    assert_eq!(resp[0]["error"]["message"], "Method not found");
    assert!(resp[0].get("result").is_none());
}

#[test]
fn malformed_input_lines_are_skipped_not_fatal() {
    let repo = fixture_repo("mcp-malformed");
    let (stdout, _stderr, code) = run(
        &repo,
        &["--mcp"],
        &[],
        Some("not json at all\n\n{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/list\",\"params\":{}}\n"),
    );
    assert_eq!(code, 0, "server must survive garbage and keep serving");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let parsed: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(parsed["id"], 11);
}
