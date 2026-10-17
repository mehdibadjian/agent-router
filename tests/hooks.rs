//! Integration tests for native lifecycle hook mode (`agent-router --hook`):
//! - VS Code Copilot & Claude Code (`UserPromptSubmit` -> `hookSpecificOutput.additionalContext`)
//! - Antigravity / AGY (`PreInvocation` -> `injectSteps[].ephemeralMessage`)

mod common;

use common::fixture_repo;
use common::run;
use serde_json::{json, Value};

const OFFLINE: &[(&str, &str)] = &[
    ("ROUTER_LLM_URL", "http://127.0.0.1:1/v1/chat/completions"),
    ("ROUTER_TIMEOUT_MS", "150"),
];

#[test]
fn copilot_and_claude_user_prompt_submit_hook_emits_additional_context() {
    let repo = fixture_repo("hook-copilot-claude");
    let stdin_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "cwd": repo,
        "prompt": "fix rust lifetime in parser and add test"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON output");
    assert_eq!(resp["continue"], true);
    assert!(resp["systemMessage"]
        .as_str()
        .unwrap_or_default()
        .contains("⚡ AgentRouter ["));
    assert_eq!(
        resp["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("subagent=\"rust-architect\""));
    assert!(ctx.contains("skills=\"skills/tdd/SKILL.md\""));
    assert!(ctx.contains("<routing_directive>"));
    assert!(ctx.contains("MANDATORY SUBAGENT DELEGATION: You MUST delegate this task to the custom agent `rust-architect`"));
    assert!(ctx.contains("runSubagent"));
    assert!(ctx.contains("MANDATORY SKILLS: Before writing code, read and follow the skill instructions at: `skills/tdd/SKILL.md`"));
    assert!(ctx.contains("<objective>"));
}

#[test]
fn agy_pre_invocation_hook_reads_transcript_and_emits_inject_steps() {
    let repo = fixture_repo("hook-agy");
    let transcript_path = format!("{}/transcript.jsonl", repo);
    let transcript_lines = [
        json!({"step_index": 0, "type": "USER_INPUT", "content": "initial prompt"}).to_string(),
        json!({"step_index": 1, "type": "PLANNER_RESPONSE", "content": "done"}).to_string(),
        json!({"step_index": 2, "type": "USER_INPUT", "content": "fix rust lifetime in parser"}).to_string(),
        // Partially flushed / malformed trailing line must be skipped without aborting reverse scan
        "{\"truncated_json_line\": ".to_string(),
    ]
    .join("\n");
    std::fs::write(&transcript_path, transcript_lines).expect("write transcript");

    let stdin_payload = json!({
        "conversationId": "conv-123",
        "workspacePaths": [repo],
        "transcriptPath": transcript_path,
        "invocationNum": 2
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid AGY hook JSON");
    let msg = resp["injectSteps"][0]["ephemeralMessage"]
        .as_str()
        .expect("ephemeralMessage string");
    assert!(msg.contains("subagent=\"rust-architect\""));
    assert!(msg.contains("<routing_directive>"));
    assert!(msg.contains("<objective>"));
}

#[test]
fn hook_blocks_and_saves_handoff_when_ceiling_exceeded_then_injects_on_new_session() {
    let repo = fixture_repo("hook-reset");
    let (_s, _e, code) = run(&repo, &["--sync-tokens", "36000"], OFFLINE, None);
    assert_eq!(code, 0);

    let stdin_payload = json!({
        "hookEventName": "UserPromptSubmit",
        "sessionId": "vscode-session-1",
        "cwd": repo,
        "prompt": "implement rust parser"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON");
    assert_eq!(resp["continue"], false);
    assert_eq!(resp["decision"], "block");
    assert!(resp["stopReason"]
        .as_str()
        .unwrap_or_default()
        .contains(">35% ceiling"));
    assert!(resp["reason"]
        .as_str()
        .unwrap_or_default()
        .contains(">35% ceiling"));
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("action=\"START_NEW_SESSION\""));
    assert!(ctx.contains("<session_handoff>"));

    // Verify `.router/HANDOFF.md` was persisted for the fresh session
    let handoff_path = format!("{}/.router/HANDOFF.md", repo);
    assert!(
        std::path::Path::new(&handoff_path).exists(),
        "HANDOFF.md must be written when >35% reset triggers"
    );

    // Next turn on a fresh host session (`session_id` changed after `/clear` or New Chat `+`) consumes HANDOFF.md
    let fresh_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "sessionId": "fresh-session-2",
        "cwd": repo,
        "prompt": "implement rust parser"
    })
    .to_string();

    let (stdout2, _stderr2, code2) = run(&repo, &["--hook"], OFFLINE, Some(&fresh_payload));
    assert_eq!(code2, 0);
    let resp2: Value = serde_json::from_str(stdout2.trim()).expect("valid hook JSON");
    assert_eq!(resp2["continue"], true);
    assert!(resp2.get("decision").is_none(), "fresh session must not block");
    let ctx2 = resp2["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx2.contains("action=\"CONTINUE_SESSION\""));
    assert!(
        ctx2.contains("<session_handoff>"),
        "fresh session first turn must inject saved handoff"
    );
    assert!(
        !std::path::Path::new(&handoff_path).exists(),
        "HANDOFF.md must be deleted after consumption"
    );
}

#[test]
fn transcript_path_token_measurement_triggers_ceiling_without_manual_sync() {
    let repo = fixture_repo("hook-transcript-tokens");
    let transcript_path = format!("{}/bloated.jsonl", repo);
    // ~140,000 chars -> ~38,889 tokens + 10,000 base overhead > 44,800 (35% of 128k)
    let bloated_line = json!({
        "type": "ASSISTANT_RESPONSE",
        "content": "x".repeat(140_000)
    })
    .to_string();
    std::fs::write(&transcript_path, bloated_line).expect("write bloated transcript");

    let stdin_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "sess-bloated",
        "transcript_path": transcript_path,
        "cwd": repo,
        "prompt": "fix rust lifetime in parser"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON");
    assert_eq!(resp["continue"], false);
    assert_eq!(resp["decision"], "block");
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("action=\"START_NEW_SESSION\""));
}

#[test]
fn github_agents_and_skills_directories_are_discovered() {
    let repo = fixture_repo("hook-github-dirs");
    std::fs::create_dir_all(format!("{}/.github/agents", repo)).unwrap();
    std::fs::create_dir_all(format!("{}/.github/skills/gh-actions", repo)).unwrap();

    // Omit `name:` in frontmatter to verify `.agent.md` double-extension stripping
    std::fs::write(
        format!("{}/.github/agents/ci-bot.agent.md", repo),
        "---\ndescription: GitHub Actions specialist\ntriggers: [workflow, pipeline]\n---\n",
    )
    .unwrap();
    std::fs::write(
        format!("{}/.github/skills/gh-actions/SKILL.md", repo),
        "---\nname: gh-actions\ndescription: Actions YAML rules\ntriggers: [workflow]\n---\n",
    )
    .unwrap();

    let stdin_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "cwd": repo,
        "prompt": "fix workflow pipeline"
    })
    .to_string();

    let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(&stdin_payload));
    assert_eq!(code, 0);

    let resp: Value = serde_json::from_str(stdout.trim()).expect("valid hook JSON");
    let ctx = resp["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(ctx.contains("subagent=\"ci-bot\""));
    assert!(ctx.contains("skills=\".github/skills/gh-actions/SKILL.md\""));
}

#[test]
fn malformed_or_empty_hook_input_fails_open_cleanly() {
    let repo = fixture_repo("hook-malformed");
    for payload in ["", "   \n  ", "not json at all", "{\"no_prompt\": true}"] {
        let (stdout, _stderr, code) = run(&repo, &["--hook"], OFFLINE, Some(payload));
        assert_eq!(code, 0, "hook must exit 0 on malformed/empty input");
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("must emit valid JSON");
        assert_eq!(parsed, json!({}));
    }
}
