# agent-router

Zero-runtime-dependency prompt pre-processor and subagent dispatcher for AI coding harnesses.

A single static Rust binary that sits in front of an expensive "System 2" coding model and does the cheap work first: it enforces a hard context-saturation ceiling, compiles terse prompts into structured engineering specs, indexes the repository's agents and skills with a sub-millisecond cache, and routes the result to the best-matched subagent — without Python, Node, or Docker on the host.

**Status:** Phases 1–4 complete. CI is green on all six platform jobs, including the static-linkage assertion.

Docs: [Business Requirements](docs/BRD.md) · [Implementation Plan](docs/IMPLEMENTATION_PLAN.md)

## How it works

```
raw prompt → Step 0 budget check → Step 2 registry cache → Step 1 compile+route → Step 3 dispatch
```

| Step | Module | Role |
|---|---|---|
| 0 | `src/budget.rs` | Estimates tokens with no BPE library; syncs with live harness `transcript_path` token counts; sets `must_reset_session` when projected usage exceeds 35% of the window; maintains turn-by-turn `rolling_summary` breadcrumbs and `.router/HANDOFF.md` state handoffs. |
| 2 | `src/registry.rs` | Walks agent, skill, prompt, and instruction directories, parses YAML frontmatter with a hand-rolled reader (stripping `.agent.md`, `.prompt.md`, and `.instructions.md` double extensions), and caches `.router/registry.json` behind an XXH3 fingerprint of paths + nanosecond mtimes. |
| 1 | `src/compiler.rs` | Calls any OpenAI-compatible endpoint to rewrite the prompt into `<objective>` / `<constraints>` / `<verification>`. Bypasses the call for trivial commands. Rejects invented agent IDs. Falls open on any failure. |
| 3 | `src/dispatcher.rs` | Emits the envelope via CLI subprocess (`Mode A`) or clean stdout JSON (`Mode B`). |
| 3C | `src/hook.rs` | Native lifecycle hook adapter (`--hook`) reading stdin JSON and emitting harness-native context injection (`<routing_directive>` with `runSubagent` delegation + live `systemMessage` status banner, or hard 35% circuit-breaker block) for VS Code Copilot, Claude Code, and Antigravity (AGY). |

## Build

Requires only a Rust toolchain (tested on 1.98.1, edition 2021).

```bash
cargo build --release
cargo test --all-targets
```

Cross-compile statically (per `.cargo/config.toml`):

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Usage

```bash
agent-router "<raw prompt>"                       # Mode B: envelope JSON to stdout, logs to stderr
agent-router --hook                               # Mode C: lifecycle hook (reads stdin JSON, writes hook JSON to stdout)
agent-router --sync-tokens <exact_count>          # calibrate the ledger from real API usage
```

Route a prompt directly and inspect the contract it produces:

```bash
agent-router "fix rust lifetime in parser and add test"
```

```json
{
  "session_meta": { "session_id": 1, "saturation_ratio": 0.078, "threshold_exceeded": false, "action": "CONTINUE_SESSION" },
  "routing": {
    "passthrough": false,
    "target_subagent": "rust-architect",
    "target_subagent_path": ".agents/rust-architect.md",
    "required_skills": ["skills/tdd/SKILL.md"]
  },
  "payload": { "context_handoff_summary": null, "compiled_prompt": "<objective>\n...\n</objective>\n..." }
}
```

Wrap an existing harness (`Mode A`). The compiled prompt is passed as an argument, the envelope is not printed, and the harness exit code is propagated:

```bash
ROUTER_HARNESS_BIN=claude agent-router "add pagination to /users"
```

## Hook Integration (`--hook`)

Instead of running a stateful MCP server, `agent-router --hook` integrates directly with native lifecycle hooks in **VS Code GitHub Copilot**, **Claude Code**, and **Antigravity (AGY)**:

### 1. VS Code GitHub Copilot (`.github/hooks/agent-router.json` & `.vscode/settings.json`)

`.github/hooks/agent-router.json`:
```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "type": "command",
        "command": "./target/release/agent-router --hook",
        "timeout": 5000
      }
    ]
  }
}
```

`.vscode/settings.json`:
```json
{
  "chat.useHooks": true,
  "chat.customAgentInSubagent.enabled": true
}
```

### 2. Claude Code (`.claude/settings.json`)

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "agent-router --hook"
          }
        ]
      }
    ]
  }
}
```

### 3. Antigravity / AGY (`.agents/hooks.json`)

```json
{
  "agent-router": {
    "PreInvocation": [
      {
        "type": "command",
        "command": "agent-router --hook",
        "timeout": 5
      }
    ]
  }
}
```

### Hook Runtime Behavior & 35% Circuit-Breaker Lifecycle

- **Normal Turn (`<= 35%` context window):**
  - Displays a live status banner in the chat window via `systemMessage`: `⚡ AgentRouter [8.0%/35% ctx | subagent: rust-architect | skills: skills/tdd/SKILL.md]`
  - Injects `<router_meta ... />`, an imperative `<routing_directive>` (instructing VS Code Copilot to invoke `runSubagent` for the matched custom agent and read all matched skills/prompts/instructions), and the compiled `<objective>` / `<constraints>` / `<verification>` specification via `hookSpecificOutput.additionalContext`.
  - Appends a 1-line breadcrumb of the turn into `session.rolling_summary` (capped at 600 bytes UTF-8 safely).
- **Hard Ceiling Turn (`> 35%` context window):**
  - Blocks execution in the bloated chat (`"continue": false`, `"decision": "block"`, `"stopReason"` / `"reason"`), persists the compressed multi-turn handoff to `.router/HANDOFF.md`, and keeps `accumulated_tokens` pinned so any retry in the same bloated `sessionId` remains blocked.
  - As soon as the user starts a new chat (`+` / `/clear`, changing `sessionId`), `accumulated_tokens` resets to `0`, and `.router/HANDOFF.md` is automatically injected as `<session_handoff>` on the first non-passthrough coding turn and then deleted.

## Configuration

All configuration is via environment variables. API keys are read from the environment only and are never written to `.router/` state or passed as process arguments.

| Variable | Default | Purpose |
|---|---|---|
| `ROUTER_LLM_URL` | `http://127.0.0.1:11434/v1/chat/completions` | Any OpenAI-compatible endpoint (Ollama, llama.cpp, vLLM, MLX, Groq, DeepSeek, Gemini). |
| `ROUTER_LLM_MODEL` | `qwen2.5-coder:7b` | Model name sent to the endpoint. |
| `ROUTER_LLM_KEY` | *(empty)* | Bearer token; omitted from the request when unset. |
| `ROUTER_TIMEOUT_MS` | `3000` | Connect **and** read timeout. Exceeding it triggers fail-open. |
| `ROUTER_BASE_OVERHEAD_TOKENS` | `10000` | Hidden tokens the harness injects (system prompt, tool defs, rules). |
| `ROUTER_MAX_TOKENS` | `128000` | Denominator for the saturation ratio. |
| `ROUTER_HARNESS_BIN` | *(unset)* | When set, enables Mode A subprocess dispatch. |

Injected into the spawned harness under Mode A: `ROUTED_SESSION_ID`, `ROUTED_SESSION_ACTION`, `ROUTED_SUBAGENT`, `ROUTED_SUBAGENT_PATH`, `ROUTED_SKILLS`.

## Discovery conventions

Scanned for `.md` manifests, with a hand-written frontmatter parser (no `serde_yaml`) reading only `name`/`id`, `description`, and `triggers`:

| Directory | Kind | Notes |
|---|---|---|
| `.agents/` | `agent` | Standard agent definitions |
| `.claude/agents/` | `agent` | Claude Code subagents |
| `.github/agents/` | `agent` | VS Code Copilot custom agents (strips `.agent.md`) |
| `skills/` | `skill` | Standard `SKILL.md` skills |
| `.claude/skills/` | `skill` | Claude Code skills |
| `.github/skills/` | `skill` | GitHub Copilot skills |
| `.github/prompts/` | `skill` | VS Code Copilot reusable prompts (strips `.prompt.md`) |
| `.github/instructions/` | `skill` | VS Code Copilot custom instructions (strips `.instructions.md`) |

`SKILL.md` files inherit their ID from the containing directory when no `name` is declared. Repositories with zero agents and only skills/prompts/instructions route cleanly with `target_subagent = "default"` while injecting all matched skill/prompt/instruction paths into the main agent's context.

## Dependencies

Exactly five crates, pinned in `Cargo.lock`; `ureq` is configured `default-features = false, features = ["tls", "json"]` so TLS is pure-Rust (`rustls` + `ring`) rather than OpenSSL. No async runtime, no tokenizer C library, no clap.

```
serde 1.0.229 · serde_json 1.0.154 · ureq 2.12.1 · xxhash-rust 0.8.19 · walkdir 2.5.0
```

State lives in `.router/session.json`, `.router/registry.json`, and (during session handoffs) `.router/HANDOFF.md`; `.router/` is gitignored and disposable.

## Design constraints worth knowing

- **Stdout is a protocol surface.** Diagnostics go to stderr exclusively, so Mode B and `--hook` output stays machine-parseable.
- **Fail-open is unconditional.** An unreachable endpoint, a timeout, or malformed LLM/hook JSON logs a warning and routes heuristically — the pipeline never blocks the developer.
- **Hallucinated IDs are stripped.** Anything the model returns that is not a registered agent/skill of the correct `kind` is discarded, falling back to `default`.
- **`panic = "abort"` is deliberately narrow.** The release profile aborts on panic, so an `unwrap()` on externally-controlled data would hard-crash rather than fail open. Byte-offset slicing on untrusted UTF-8 is a real hazard here — `registry.rs` and `budget.rs` snap to `is_char_boundary`, and `tests/pipeline.rs` guards the fail-open paths against exit code 134.

## Verification status

| Gate | Result |
|---|---|
| `cargo test --all-targets` | **19 passing** (5 unit, 6 hook integration, 8 pipeline integration) |
| Linux x86_64 musl | `ldd`: `not a dynamic executable`; `readelf` NEEDED empty; **1.83 MiB** |
| Linux aarch64 musl | `ldd`: `not a dynamic executable`; **1.56 MiB** |
| Windows msvc-static | builds with `crt-static`; **1.56 MiB** |
| macOS universal | `lipo` fat file contains `x86_64 arm64`; `otool -L` free of libssl/libcrypto/sqlite3; **2.83 MiB** |
| Fail-open smoke | routes to `.agents/rust-architect.md` with no LLM reachable, on Linux and Windows |

The 5 MiB and zero-dynamic-dependency NFRs are satisfied with room to spare.
Measured locally on the glibc host: cold and warm invocation ~1 ms with a
stable XXH3 cache fingerprint, peak RSS ~8 MiB (NFR < 15 MB).

Test coverage spans the budget ceiling math, frontmatter parsing, `.agent.md` / `.prompt.md` / `.instructions.md` suffix stripping, multibyte truncation, Copilot/Claude `UserPromptSubmit` hooks, Antigravity `PreInvocation` hooks, `transcript_path` real-time token measurement, 35% hard block + `.router/HANDOFF.md` persistence across blocked retries and passthrough turns into a fresh `sessionId`, `.github/` discovery conventions, malformed hook input survival, fail-open, fence stripping, hallucination sanitising, the passthrough gate, Mode A exit-code and env propagation, cache invalidation, and clean-repo `--sync-tokens` calibration.

## CI & Releases

`.github/workflows/ci.yml` gates `cargo test --all-targets`, builds and asserts the NFRs per platform (Linux musl x86_64/aarch64 `ldd` + `readelf` forbidden-library scan + 5 MiB ceiling, Windows msvc-static, and macOS universal binary created with `lipo`), and runs `release-please` to automatically cut semantic-versioned GitHub Releases (`v0.3.0`) with all cross-compiled static binaries attached.

## License

Not yet specified.
