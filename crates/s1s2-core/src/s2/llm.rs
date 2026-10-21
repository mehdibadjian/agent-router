//! Optional System 2 external adjudicator transport (feature = `llm`).
//!
//! Network I/O lives in exactly one place in the framework, behind one cargo feature, so
//! an extension that never escalates to a remote model compiles with no HTTP stack at all.
//! The endpoint contract is plain OpenAI-compatible JSON over `ureq` — no async runtime,
//! no OpenSSL, no C bindings.

use std::time::Duration;

/// POST one chat-completion request and return the parsed response body.
///
/// `temperature` is pinned to 0: a deliberative adjudicator that varies run to run would
/// break System 2's determinism guarantee.
pub fn chat_completion(
    url: &str,
    api_key: &str,
    model: &str,
    timeout_ms: u64,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<serde_json::Value, String> {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let body = serde_json::json!({
        "model": model,
        "temperature": 0.0,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_prompt }
        ]
    });

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build();

    let mut request = agent.post(url).set("Content-Type", "application/json");
    if !api_key.is_empty() {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }

    request
        .send_json(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())
}

/// Pull `choices[0].message.content` out of a chat-completion response.
pub fn assistant_content(resp: &serde_json::Value) -> Result<&str, String> {
    resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "Missing choices[0].message.content".to_string())
}
