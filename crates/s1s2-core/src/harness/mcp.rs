//! Zero-dependency MCP (Model Context Protocol) JSON-RPC 2.0 stdio server.
//!
//! Framing is newline-delimited JSON-RPC over stdin/stdout, which is what every
//! harness-side MCP client speaks today. Only four request methods are implemented:
//! `initialize`, `tools/list`, `tools/call`, and `ping`. Notifications get no response.
//!
//! The server is transport-only: tools are supplied by the caller, so the core stays
//! independent of any domain.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC error carried out of a tool handler.
#[derive(Debug, Clone)]
pub struct McpError {
    pub code: i64,
    pub message: String,
}

impl McpError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }
}

/// A tool exposed to MCP clients. `handler` receives the decoded `arguments` object.
pub struct McpTool {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub handler: Box<dyn FnMut(Value) -> Result<Value, McpError>>,
}

impl McpTool {
    pub fn new(
        name: &'static str,
        description: &'static str,
        input_schema: Value,
        handler: impl FnMut(Value) -> Result<Value, McpError> + 'static,
    ) -> Self {
        Self { name, description, input_schema, handler: Box::new(handler) }
    }
}

/// Newline-delimited JSON-RPC 2.0 server over stdio.
pub struct McpServer {
    server_name: String,
    server_version: String,
    tools: Vec<McpTool>,
}

impl McpServer {
    pub fn new(server_name: &str, server_version: &str) -> Self {
        Self {
            server_name: server_name.to_string(),
            server_version: server_version.to_string(),
            tools: Vec::new(),
        }
    }

    pub fn tool(mut self, tool: McpTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Read requests from stdin and answer on stdout until EOF.
    pub fn serve_stdio(&mut self) -> io::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(response) = self.handle_line(trimmed) {
                writeln!(out, "{response}")?;
                out.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one framed request. `None` means "send nothing" (a notification).
    pub fn handle_line(&mut self, raw: &str) -> Option<String> {
        let request: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => return Some(error(Value::Null, -32700, &format!("Parse error: {e}"))),
        };

        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        // JSON-RPC 2.0: a request with no `id` is a notification and is never answered,
        // whatever its method.
        let id = request.get("id").cloned()?;

        let payload = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": self.server_name,
                    "version": self.server_version,
                },
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": self.tool_descriptors() }),
            "tools/call" => return Some(self.call_tool(&id, &request)),
            "" => return Some(error(id, -32600, "Invalid Request")),
            other => {
                return Some(error(
                    id,
                    -32601,
                    &format!("Method not found: {other}"),
                ))
            }
        };

        Some(response(id, payload))
    }

    fn tool_descriptors(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect()
    }

    fn call_tool(&mut self, id: &Value, request: &Value) -> String {
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        if name.is_empty() {
            return error(id.clone(), -32602, "Missing tools/call params.name");
        }

        let index = match self.tools.iter().position(|t| t.name == name) {
            Some(i) => i,
            None => {
                return error(id.clone(), -32602, &format!("Unknown tool: {name}"))
            }
        };

        let (payload, is_error) = match (self.tools[index].handler)(arguments) {
            Ok(payload) => (payload, false),
            Err(err) => (json!({ "error": err.message }), true),
        };

        let text = if is_error {
            payload["error"].as_str().unwrap_or_default().to_string()
        } else {
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| String::new())
        };

        response(
            id.clone(),
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error,
            }),
        )
    }
}

fn response(id: Value, payload: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": payload,
    }))
    .expect("response is always serializable")
}

fn error(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
    .expect("error is always serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_server() -> McpServer {
        McpServer::new("test-server", "2.0.0").tool(McpTool::new(
            "route_intent",
            "Echo the arguments back.",
            json!({
                "type": "object",
                "properties": { "raw_prompt": { "type": "string" } },
                "required": ["raw_prompt"],
            }),
            |args| Ok(json!({ "echoed": args })),
        ))
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).expect("valid json")
    }

    #[test]
    fn initialize_announces_capabilities_and_server_info() {
        let mut server = echo_server();
        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
            )
            .expect("initialize must be answered");
        let value = parse(&reply);
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(value["result"]["capabilities"]["tools"].is_object());
        assert_eq!(value["result"]["serverInfo"]["name"], "test-server");
    }

    #[test]
    fn notifications_are_never_answered() {
        let mut server = echo_server();
        assert_eq!(
            server.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
    }

    #[test]
    fn tools_list_exposes_schemas() {
        let mut server = echo_server();
        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .expect("list reply");
        let value = parse(&reply);
        let tools = value["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "route_intent");
        assert_eq!(tools[0]["inputSchema"]["required"][0], "raw_prompt");
    }

    #[test]
    fn tools_call_wraps_payload_in_text_content() {
        let mut server = echo_server();
        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"route_intent","arguments":{"raw_prompt":"fix rust"}}}"#,
            )
            .expect("call reply");
        let value = parse(&reply);
        assert_eq!(value["id"], 3);
        assert_eq!(value["result"]["isError"], false);
        let text = value["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("fix rust"), "{text}");
    }

    #[test]
    fn tool_failure_is_a_result_not_a_jsonrpc_error() {
        let mut server = McpServer::new("s", "1").tool(McpTool::new(
            "boom",
            "Always fails.",
            json!({"type": "object"}),
            |_| Err(McpError::invalid_params("raw_prompt required")),
        ));
        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"boom","arguments":{}}}"#,
            )
            .expect("reply");
        let value = parse(&reply);
        assert_eq!(value["result"]["isError"], true);
        assert!(value["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("raw_prompt required"));
    }

    #[test]
    fn unknown_method_and_malformed_frames_use_jsonrpc_errors() {
        let mut server = echo_server();
        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":5,"method":"tools/nope"}"#)
            .expect("error reply");
        let value = parse(&reply);
        assert_eq!(value["error"]["code"], -32601);

        let reply = server.handle_line("{not json").expect("parse error reply");
        assert_eq!(parse(&reply)["error"]["code"], -32700);

        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{}}"#)
            .expect("missing name");
        assert_eq!(parse(&reply)["error"]["code"], -32602);

        let reply = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"ghost"}}"#,
            )
            .expect("unknown tool");
        assert_eq!(parse(&reply)["error"]["code"], -32602);
    }

    #[test]
    fn ping_is_answered_with_an_empty_result() {
        let mut server = echo_server();
        let reply = server.handle_line(r#"{"jsonrpc":"2.0","id":8,"method":"ping"}"#).unwrap();
        assert_eq!(parse(&reply)["result"], json!({}));
    }
}
