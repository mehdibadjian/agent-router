//! Subprocess wrapper and JSON stdout dispatcher (spec §5, generalized from `dispatcher.rs`).
//!
//! Two dispatch modes:
//! * **Mode A (`spawn`)** — hand the compiled prompt to a downstream CLI harness,
//!   passing routing metadata through `ROUTED_*` environment variables, and propagate
//!   the child's exit code.
//! * **Mode B (`emit_json`)** — print the payload to stdout as pretty JSON. Diagnostics
//!   must never share stdout, so harnesses can pipe it straight into a parser.

use serde::Serialize;
use std::io::Write;
use std::process::{Command, Stdio};

/// One `ROUTED_*` environment binding for the child harness.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchEnv {
    pub key: String,
    pub value: String,
}

impl DispatchEnv {
    pub fn new(key: &str, value: &str) -> Self {
        Self { key: key.to_string(), value: value.to_string() }
    }
}

/// Everything Mode A needs: the binary to spawn, the prompt to pass, and the env block.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub program: String,
    pub prompt: String,
    pub args: Vec<String>,
    pub env: Vec<DispatchEnv>,
}

impl SpawnRequest {
    pub fn new(program: &str, prompt: &str) -> Self {
        Self { program: program.to_string(), prompt: prompt.to_string(), args: Vec::new(), env: Vec::new() }
    }

    pub fn arg(mut self, arg: &str) -> Self {
        self.args.push(arg.to_string());
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        if !value.is_empty() {
            self.env.push(DispatchEnv::new(key, value));
        }
        self
    }
}

/// Run the child harness with inherited stdio, returning its exit code.
///
/// A spawn failure is reported as `Err` so the caller can fail open rather than
/// silently pretending the child ran.
pub fn spawn(request: &SpawnRequest) -> Result<i32, String> {
    let mut command = Command::new(&request.program);
    command
        .envs(request.env.iter().map(|e| (e.key.as_str(), e.value.as_str())))
        .args(&request.args)
        .arg(&request.prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command
        .status()
        .map_err(|e| format!("Failed to spawn harness '{}': {}", request.program, e))?;
    Ok(status.code().unwrap_or(0))
}

/// Mode B: serialize `payload` as pretty JSON and write it to stdout in one call.
pub fn emit_json<T: Serialize>(payload: &T) -> Result<(), String> {
    let body = serde_json::to_string_pretty(payload).map_err(|e| e.to_string())?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{body}").map_err(|e| e.to_string())?;
    handle.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Serialize without printing, for callers that want the bytes (MCP text content).
pub fn to_json_string<T: Serialize>(payload: &T) -> Result<String, String> {
    serde_json::to_string(payload).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_builder_skips_empty_values() {
        let req = SpawnRequest::new("harness", "prompt")
            .env("ROUTED_SUBAGENT", "rust-architect")
            .env("ROUTED_SKILLS", "");
        assert_eq!(req.env.len(), 1, "empty values must not be injected");
        assert_eq!(req.env[0].key, "ROUTED_SUBAGENT");
        assert_eq!(req.env[0].value, "rust-architect");
    }

    #[test]
    fn spawn_reports_missing_program_as_error() {
        let req = SpawnRequest::new("/nonexistent/agent-router-harness-binary", "p");
        let err = spawn(&req).expect_err("missing program must fail");
        assert!(err.contains("Failed to spawn"), "{err}");
    }

    #[test]
    fn spawn_propagates_child_exit_code() {
        let req = SpawnRequest::new("true", "p");
        assert_eq!(spawn(&req).expect("true must exit 0"), 0);
        let req = SpawnRequest::new("sh", "p").arg("-c").arg("exit 7");
        // `sh -c "exit 7" p`: the trailing prompt arg is $0, so the exit code is 7.
        assert_eq!(spawn(&req).expect("child exit code"), 7);
    }

    #[test]
    fn json_helpers_round_trip() {
        let payload = serde_json::json!({"routing": {"target_subagent": "rust-architect"}});
        let compact = to_json_string(&payload).expect("serialize");
        assert!(compact.contains("\"target_subagent\""), "{compact}");
        assert!(!compact.contains('\n'), "compact form must be single-line");
    }
}
