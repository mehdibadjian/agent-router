# agent-router

Zero-runtime-dependency System 1 / System 2 cognitive pre-processor and subagent dispatcher for AI coding harnesses.

A single static Rust binary that sits in front of an expensive "System 2" coding model and does the cheap work first: it enforces a hard context-saturation ceiling, compiles terse prompts into structured engineering specs, indexes the repository's agents and skills with a sub-millisecond cache, and routes the result to the best-matched subagent — without Python, Node, or Docker on the host.

As of **v2.0.0** the routing logic is no longer the architecture. The engine is a reusable crate (`s1s2-core`), developer subagent routing is its first extension (`ext-agent-router`), and the binary is a thin harness over both. Every v0.3.0 CLI flag, stdout envelope, `ROUTED_*` block, hook JSON payload, and `.router/` state file is still produced byte-for-byte; the v2 surface is additive.

Docs: [Business Requirements](docs/BRD.md) · [Implementation Plan](docs/IMPLEMENTATION_PLAN.md)

## Architecture

```
raw prompt ──► System 1 (<5ms, no I/O) ──► Metacognitive Gate ──► System 2 (deliberative)
                     ▲                        │  FastCommit / Escalate / Halt
                     │                              ▼
             xxh64 mtime registry cache      deterministic handoff artifact
```

| Stage | Owner | Contract |
|---|---|---|
| Engine coordinator | `s1s2-core::CognitiveEngine<D>` | Runs `budget → System 1 → gate → System 2`, owns the cache, times every turn, stamps the pathway. |
| Domain interface | `s1s2-core::domain::CognitiveDomain` | Six static-dispatch methods (`build_registry`, `measure_budget`, `execute_system1`, `evaluate_gate`, `execute_system2`, `handle_circuit_trip`) over five associated types. No `dyn`, no allocations for dispatch. |
| Gate | `s1s2-core::gate::MetacognitiveGate` | Emits `GateVerdict::{FastCommit, Escalate(reason), Halt(trip)}` from confidence, candidate count, saturation, and an explicit System 2 request. |
| Circuit breaker | `s1s2-core::s1::circuit` | 30% soft warning, 35% hard ceiling, dependency-free token estimate. |
| Registry cache | `s1s2-core::s1::cache::MtimeCache` | xxh64 fingerprint over sorted `path:mtime_nanos:size` lines; warm path deserializes instead of reparsing. |
| System 2 | `s1s2-core::s2` | XML envelope builder (entity-escaped, injection-contained), `ureq` adjudicator behind the `llm` feature, handoff artifact renderer. |
| Harness adapters | `s1s2-core::harness` | `cli` (spawn + stdout JSON) and `mcp` (newline-delimited JSON-RPC 2.0 stdio server). |
| Extension #1 | `ext-agent-router` | The developer subagent routing domain: registry discovery, heuristic scoring, prompt compilation, ledger-aware budget. |
| Binary | `bins/agent-router` | Argument dispatch, `.router/` ledger state, envelope/hook/MCP wire formats. Holds no cognitive policy of its own. |

### Binary internals (`bins/agent-router`)

| Module | Role |
|---|---|
| `src/main.rs` | Flag dispatcher: `--hook`, `--mcp-stdio`, `--sync-tokens`, `--prompt`, `--wrap`, legacy positional. |
| `src/pipeline.rs` | One turn in, one envelope out: loads the ledger, delegates to the engine, persists `.router/session.json` and `.router/HANDOFF.md`. |
| `src/envelope.rs` | The v0.3.0 three-section wire format plus the additive `cognitive` block and the `ROUTED_*` env map. |
| `src/hook.rs` | Native lifecycle hook adapter (`--hook`) reading stdin JSON and emitting harness-native context injection for VS Code Copilot, Claude Code, and Antigravity (AGY). |
| `src/mcp.rs` | MCP stdio server: `route_intent`, `check_context_budget`, `refresh_registry`. |

## Build

Requires only a Rust toolchain (tested on 1.98.1, edition 2021).

```bash
cargo build --release
cargo test --workspace --all-targets
```

Cross-compile statically (per `.cargo/config.toml`) — the workspace layout does not change the artifact path:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Usage

```bash
agent-router "<raw prompt>"                       # Mode B: envelope JSON to stdout, logs to stderr
agent-router --prompt "<raw prompt>"              # explicit prompt flag; reads stdin when omitted
agent-router --prompt                             # prompt from stdin
agent-router --hook                               # Mode C: lifecycle hook (stdin JSON -> hook JSON)
agent-router --mcp-stdio                          # MCP server over newline-delimited JSON-RPC
agent-router --wrap <CMD> [args...]               # route, then exec CMD with the ROUTED_* env block
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
  "payload": { "context_handoff_summary": null, "compiled_prompt": "<objective>\n...\n</objective>\n..." },
  "cognitive": { "pathway": "system1_fast", "escalation": null, "latency_micros": 276, "cache_hit": false }
}
```

`session_meta` / `routing` / `payload` are unchanged from v0.3.0. `cognitive` is new and additive; strict parsers ignore it. Its `pathway` is one of `system1_fast`, `system2_deliberative:<reason>`, or `circuit_breaker_halt:<trip>`.

Wrap an existing harness (`Mode A`, env-driven or explicit). The compiled prompt is passed as an argument, the envelope is not printed, and the harness exit code is propagated:

```bash
ROUTER_HARNESS_BIN=claude agent-router "add pagination to /users"
agent-router --wrap claude "fix rust lifetime bug"
```

## MCP server (`--mcp-stdio`)

Newline-delimited JSON-RPC 2.0 over stdio — no HTTP server, no ports. A request without an `id` is a notification and gets no reply. Tool-handler failures return `isError: true` content (transport stays healthy); JSON-RPC error codes are reserved for protocol faults (`-32700` parse, `-32601` method not found, `-32602` unknown tool).

| Tool | Arguments | Result |
|---|---|---|
| `route_intent` | `raw_prompt` (required), `context_tokens_used`, `context_window_limit`, `force_compile` | The same envelope `--prompt` prints, as JSON text content. Commits the turn to `.router/session.json`. |
| `check_context_budget` | `raw_prompt` (required), optional token/window overrides | `{saturation_ratio, used_tokens, limit_tokens, circuit_tripped, hard_ceiling_ratio, soft_warning_ratio, session_id, accumulated_tokens}`. Probes only — never compiles or commits. |
| `refresh_registry` | none | Invalidates and rebuilds `.router/registry.json` from disk, returns `{refreshed, cache_file}`. |

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
  - The blocked turn carries the machine-readable handoff artifact (`<session_ledger>` with every prior breadcrumb, budget, and pathway) so continuity survives the reset.
  - As soon as the user starts a new chat (`+` / `/clear`, changing `sessionId`), `accumulated_tokens` resets to `0`, and `.router/HANDOFF.md` is automatically injected as `<session_handoff>` on the first non-passthrough coding turn and then deleted.

## Gate rules (Extension #1)

The gate is evaluated in this order, so the first matching row wins:

| Signal | Verdict | Pathway |
|---|---|---|
| `> 35%` projected saturation | `Halt(SaturationExceeded)` | `circuit_breaker_halt:saturation-exceeded:0.375>0.350` |
| Trivial shell/test command (and no `force_compile`) | `FastCommit` (passthrough) | `system1_fast` |
| `force_compile` (`--prompt --force`, MCP `force_compile`) | `Escalate(ExplicitSystem2Request)` | `system2_deliberative:explicit-system2-request` |
| `>= 30%` saturation | `Escalate(ContextSaturationWarning)` | `system2_deliberative:context-saturation-warning:0.312` |
| `confidence < 0.85` | `Escalate(LowConfidence)` | `system2_deliberative:low-confidence:0.600/0.850` |
| `candidate_count != 1` | `Escalate(AmbiguousCandidates)` | `system2_deliberative:ambiguous-candidates:2` |
| otherwise | `FastCommit` | `system1_fast` |

Escalation runs the local deterministic compiler when no adjudicator is configured or reachable, and the `ureq` client otherwise. Any adjudicator failure logs to stderr and falls open to the deterministic path.

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

The 30%/35% thresholds are engine constants (`s1s2_core::s1::circuit::DEFAULT_SOFT_WARNING_RATIO` / `DEFAULT_HARD_CEILING_RATIO`), not environment knobs, so a misconfigured host cannot silently raise the ceiling.

Injected into the spawned harness under Mode A: `ROUTED_SESSION_ID`, `ROUTED_SESSION_ACTION`, `ROUTED_SUBAGENT`, `ROUTED_SUBAGENT_PATH`, `ROUTED_SKILLS`, and (v2) `ROUTED_PATHWAY`.

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

The same five crates, now declared once in `[workspace.dependencies]` and pinned in `Cargo.lock`; `ureq` is configured `default-features = false, features = ["tls", "json"]` so TLS is pure-Rust (`rustls` + `ring`) rather than OpenSSL. No async runtime, no tokenizer C library, no clap.

```
serde 1.0.229 · serde_json 1.0.154 · ureq 2.12.1 · xxhash-rust 0.8.19 · walkdir 2.5.0
```

`ureq` is optional in `s1s2-core` and enabled by the `llm` feature. `cargo tree -p s1s2-core` resolves to **zero** `ureq` nodes by default — an extension that never escalates to a remote adjudicator ships a network-free core.

State lives in `.router/session.json`, `.router/registry.json`, and (during session handoffs) `.router/HANDOFF.md`; `.router/` is gitignored and disposable. Both file schemas are unchanged from v0.3.0 (`{hash, entries}` for the registry; `session.json` gains `turn_count` / `last_pathway` with serde defaults, so a v0.3.0 ledger still loads).

## Writing a second extension

Implement `CognitiveDomain` in a new crate, then pick the harness adapters you need:

```rust
use s1s2_core::domain::{CircuitTrip, CognitiveDomain, EscalationReason, GateVerdict};
use s1s2_core::{CognitiveEngine, ExecutionPathway};

impl CognitiveDomain for MyDomain {
    type Input = MyInput;
    type RegistryState = MyRegistry;
    type FastOutput = MyCandidate;
    type FinalDecision = MyDecision;
    type Config = MyConfig;

    fn build_registry(config: &Self::Config) -> Result<Self::RegistryState, String> { /* … */ }
    fn measure_budget(input: &Self::Input, config: &Self::Config) -> (usize, usize) { /* … */ }
    fn execute_system1(input: &Self::Input, registry: &Self::RegistryState, config: &Self::Config)
        -> Result<Self::FastOutput, String> { /* … */ }
    fn evaluate_gate(input: &Self::Input, s1: Self::FastOutput, ratio: f32,
                     config: &Self::Config, breaker: &s1s2_core::s1::circuit::CircuitBreaker)
        -> GateVerdict<Self::FinalDecision> { /* … */ }
    fn execute_system2(input: &Self::Input, registry: &Self::RegistryState,
                       partial: Option<Self::FinalDecision>, reason: &EscalationReason,
                       config: &Self::Config) -> Result<Self::FinalDecision, String> { /* … */ }
    fn handle_circuit_trip(input: &Self::Input, trip: CircuitTrip, config: &Self::Config)
        -> Result<Self::FinalDecision, String> { /* … */ }
}

let mut engine = CognitiveEngine::<MyDomain>::new(config, Default::default());
let envelope = engine.process(&input)?;   // CognitiveEnvelope<MyDecision>
assert_eq!(envelope.pathway, ExecutionPathway::System1Fast);
```

Override `domain_id`, `watched_dirs`, `cache_file`, `registry_extensions`, and `cache_payload_key` only when the defaults (OS temp cache, `.md` manifests, `payload` key) do not fit. `crates/s1s2-core/tests/mock_domain.rs` is a minimal working second domain.

## Design constraints worth knowing

- **Stdout is a protocol surface.** Diagnostics go to stderr exclusively, so Mode B, `--hook`, and MCP framing stay machine-parseable.
- **Fail-open is unconditional.** An unreachable endpoint, a timeout, or malformed LLM/hook JSON logs a warning and routes heuristically — the pipeline never blocks the developer.
- **Hallucinated IDs are stripped.** Anything the model returns that is not a registered agent/skill of the correct `kind` is discarded, falling back to `default`.
- **Prompt injection is contained at the envelope.** User text is entity-escaped and attribute-sanitized before it reaches any XML tag, so a `<circuit_breaker_tripped>` breakout from prompt data is impossible.
- **`panic = "abort"` is deliberately narrow.** The release profile aborts on panic, so an `unwrap()` on externally-controlled data would hard-crash rather than fail open. Byte-offset slicing on untrusted UTF-8 is a real hazard here — `registry.rs` and the budget estimator snap to `is_char_boundary`, and `tests/pipeline.rs` guards the fail-open paths against exit code 134.

## Verification status

| Gate | Result |
|---|---|
| `cargo test --workspace --all-targets` | **83 passing** (36 `s1s2-core` · 24 `ext-agent-router` · 23 `agent-router`) |
| Backward compatibility | Legacy `tests/pipeline.rs` (8) and `tests/hooks.rs` (6) run unmodified against the engine-driven binary |
| v2 surface | `tests/harness.rs` (8): `--prompt`, stdin, `--wrap`, MCP handshake + all three tools, JSON-RPC error matrix, legacy positional |
| Spec acceptance tests | `ext-agent-router/tests/pathways.rs` (4): sub-5ms fast commit, escalation + deterministic recompilation, 37.5% halt with handoff artifact, mtime invalidation + legacy cache schema |
| Network-free core | `cargo tree -p s1s2-core` contains 0 `ureq` nodes with default features |
| Build warnings | Zero, on a clean `--workspace --all-targets` rebuild |
| Release binary | **1.82 MiB** (limit 5 MiB) |
| Linux x86_64 musl (CI) | `ldd`: `not a dynamic executable`; `readelf` NEEDED empty |
| Linux aarch64 musl (CI) | `ldd`: `not a dynamic executable`; **1.56 MiB** |
| Windows msvc-static (CI) | builds with `crt-static`; **1.56 MiB** |
| macOS universal (CI) | `lipo` fat file contains `x86_64 arm64`; `otool -L` free of libssl/libcrypto/sqlite3; **2.83 MiB** |
| Fail-open smoke (CI) | routes to `.agents/rust-architect.md` with no LLM reachable, on Linux and Windows |

Measured locally on the glibc host: cold and warm invocation ~1 ms with a stable xxh64 cache fingerprint, peak RSS ~8 MiB (NFR < 15 MB).

Test coverage spans the budget ceiling math, frontmatter parsing, `.agent.md` / `.prompt.md` / `.instructions.md` suffix stripping, multibyte truncation, gate truth table, breaker band edges, cache fingerprint order-independence, XML entity escaping and injection containment, handoff artifact content, ledger backward compatibility, Copilot/Claude `UserPromptSubmit` hooks, Antigravity `PreInvocation` hooks, `transcript_path` real-time token measurement, 35% hard block + `.router/HANDOFF.md` persistence across blocked retries and passthrough turns into a fresh `sessionId`, `.github/` discovery conventions, malformed hook input survival, fail-open, fence stripping, hallucination sanitising, the passthrough gate, Mode A exit-code and env propagation, cache invalidation, clean-repo `--sync-tokens` calibration, and the full MCP JSON-RPC surface.

## CI & Releases

`.github/workflows/ci.yml` gates `cargo test --workspace --all-targets` plus a default-feature network-free check on `s1s2-core`, builds and asserts the NFRs per platform (Linux musl x86_64/aarch64 `ldd` + `readelf` forbidden-library scan + 5 MiB ceiling, Windows msvc-static, and macOS universal binary created with `lipo`), and runs `release-please` to automatically cut semantic-versioned GitHub Releases with all cross-compiled static binaries attached. `CHANGELOG.md` is generated by release-please and is not hand-edited.

Release plumbing for the workspace: `[workspace.package].version` in the root `Cargo.toml` is the only version literal, and every member declares `version.workspace = true`. Because release-please's `rust` release type insists on a `[package]` manifest at the component root, the root component uses `release-type: "simple"` with a TOML extra-file updater pointed at `$.workspace.package.version`, plus one shared `vX.Y.Z` tag (`include-component-in-tag: false`). `release-please-config.json` currently also carries the one-shot `"release-as": "2.0.0"` so this migration is cut as `v2.0.0` rather than `v1.0.0`; delete that key once `v2.0.0` is tagged.

## License

Not yet specified.
