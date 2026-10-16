use crate::run_pipeline;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;

pub fn run_mcp_stdio_loop(repo_root: &Path) -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line_res in stdin.lock().lines() {
        let line = line_res?;
        if line.trim().is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Notifications (no "id") do not receive a response in JSON-RPC 2.0
        if req.get("id").is_none() {
            continue;
        }

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "agent-router", "version": "0.1.0" }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "route_and_compile",
                        "description": "Enforces 35% context ceiling, compiles raw prompt into XML engineering spec, and selects optimal repo subagent and skills.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "prompt": { "type": "string", "description": "Raw user prompt" }
                            },
                            "required": ["prompt"]
                        }
                    }]
                }
            }),
            "tools/call" => {
                let raw_prompt = req
                    .pointer("/params/arguments/prompt")
                    .and_then(|p| p.as_str())
                    .unwrap_or("");

                match run_pipeline(repo_root, raw_prompt) {
                    Ok(envelope) => {
                        let text = serde_json::to_string_pretty(&envelope).unwrap_or_default();
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": text }],
                                "isError": false
                            }
                        })
                    }
                    Err(e) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": e }],
                            "isError": true
                        }
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" }
            }),
        };

        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }
    Ok(())
}
