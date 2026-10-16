# Business Requirements Document (BRD)

**Project Name:** AgentRouter (Zero-Dependency System 1 Pre-Processor & Subagent Dispatcher)
**Document Version:** 1.0
**Date:** October 16, 2026
**Target Architecture:** Standalone Native Static Binary (Rust / Zero Runtime Dependencies)

## 1. Executive Summary & Business Case

Modern AI coding and orchestration harnesses (such as Claude Code, Cursor, LangGraph, CrewAI, and custom CLI runners) suffer from three compounding operational bottlenecks:

* **Context Window Degradation ("Lost-in-the-Middle"):** As session history grows beyond 35% saturation, frontier "System 2" models experience degraded reasoning accuracy while incurring exponential KV-cache and input token costs.
* **Unstructured User Prompts:** Terse, ambiguous developer prompts force expensive frontier models to spend multiple reasoning turns clarifying intent, grepping files, and guessing verification criteria.
* **Harness Lock-In & Fragmented Discovery:** Repositories increasingly contain specialized subagents and modular skills (`SKILL.md`, `.agents/*.md`), but discovering, caching, and routing to them is tightly coupled to specific vendor harnesses or requires heavy runtime dependencies (Python/Node.js).

AgentRouter solves this by introducing a harness-agnostic, zero-dependency native binary that intercepts raw user prompts before they hit expensive execution models. Using a free (local Ollama/MLX) or ultra-low-cost model (Gemini Flash-Lite, DeepSeek, Haiku), it enforces a hard 35% context ceiling, compiles raw prompts into deterministic engineering specifications, caches repository capabilities in sub-milliseconds, and dispatches the payload to the optimal subagent.

## 2. Business Objectives & Success Metrics (KPIs)

| Objective Category | Target Metric (KPI) | Baseline (Unrouted Harness) | Target (With AgentRouter) |
|---|---|---|---|
| Cost Efficiency | Average Frontier Token Spend per Task | 100% (Unbounded context growth) | Reduces token spend by 45% to 65% via 35% hard ceiling and prompt compilation |
| Execution Quality | First-Pass Task Completion Rate | ~60% on ambiguous prompts | Greater than 85% via injected XML constraints and verification steps |
| Portability & Ops | Host Runtime Dependencies | Requires Python/uvx, Node/npx, or Docker | Zero dependencies (Single static binary; ldd returns "not a dynamic executable") |
| System Latency | Local Binary Overhead (Excl. LLM network call) | 300ms to 1,500ms (Python/Node cold start) | Less than 10ms cold start + cache validation |
| Context Hygiene | Maximum Context Saturation Allowed | 80% to 95% (Auto-compact triggers late) | Hard limit at 35% of target model's context window |

## 3. Project Scope

### 3.1 In-Scope

* **Zero-Dependency Cross-Platform Binary:** Native compilation for Linux (x86_64 / aarch64 static musl), macOS (Universal Apple Silicon + Intel), and Windows (x86_64 static MSVC CRT).
* **Step 0 (Context Circuit Breaker):** Token ledger tracking, configurable harness overhead accounting, 35% saturation enforcement, and automated state-handoff rollups on session reset.
* **Step 1 (Prompt Compiler):** Integration with any OpenAI-compatible local or cloud endpoint to transform raw user prompts into structured specifications (Objective, Constraints, Context, Verification), plus a passthrough gate for trivial commands.
* **Step 2 (Capability Registry & Cache):** High-speed filesystem scanner and frontmatter parser for repo-local agents and skills, backed by xxh3 modification-time (mtime) hashing.
* **Step 3 (Harness-Agnostic Dispatcher):** Dual-mode execution supporting CLI subprocess wrapping, pre-prompt hook interception, and a native JSON-RPC 2.0 Model Context Protocol (MCP) stdio server.

### 3.2 Out-of-Scope

* Building a full "System 2" autonomous coding loop (AgentRouter delegates execution to existing harnesses/subagents).
* GUI or web dashboard interfaces (strictly CLI, environment variables, and stdio JSON-RPC).
* External database dependencies (all state is persisted locally in `.router/*.json`).

## 4. Functional Requirements

### 4.1 Module 0: Context Usage Monitor & 35% Circuit Breaker

| Req ID | Requirement Description | Priority | Acceptance Criteria |
|---|---|---|---|
| FR-0.1 | Zero-Dependency Token Estimation: Estimate token counts for system overhead, session history, injected skills, and the incoming prompt without external C/C++ tokenizers. | P0 | Token count calculation completes in less than 1ms with less than 3% variance against standard BPE (cl100k_base / o200k_base). |
| FR-0.2 | Configurable Harness Overhead: Account for hidden tokens injected by the underlying harness (system prompts, MCP definitions, rules files). | P0 | Supports `ROUTER_BASE_OVERHEAD_TOKENS` (default: 10000) and `ROUTER_MAX_TOKENS` (default: 128000). |
| FR-0.3 | 35% Hard Saturation Trigger: Evaluate total projected tokens against `ROUTER_MAX_TOKENS`. If usage exceeds 0.35 (35%), flag the session for mandatory reset. | P0 | Any request pushing cumulative session tokens above 35.0% sets `must_reset_session = true` deterministically. |
| FR-0.4 | Session Handoff Compression: When a reset is triggered, instruct the low-cost model to compress prior session state into a dense `<session_handoff>` artifact before clearing the ledger. | P0 | New session starts with a clean ledger (`session_id + 1`), resetting accumulated tokens while preserving active file paths, architectural decisions, and pending blockers in under 500 tokens. |

### 4.2 Module 1: Low-Cost Prompt Compiler

| Req ID | Requirement Description | Priority | Acceptance Criteria |
|---|---|---|---|
| FR-1.1 | Provider-Agnostic Cheap Model Client: Connect to local (Ollama, llama.cpp, vLLM, MLX) or low-cost cloud APIs (Gemini Flash-Lite, DeepSeek-V3, Groq) over pure-Rust TLS. | P0 | Configurable via `ROUTER_LLM_URL`, `ROUTER_LLM_KEY`, and `ROUTER_LLM_MODEL` without requiring system OpenSSL libraries. |
| FR-1.2 | Prompt Engineering Transformation: Rewrite unstructured user prompts into deterministic XML-tagged specifications (`<objective>`, `<constraints>`, `<verification>`). | P0 | Output JSON strictly adheres to schema; rewritten prompt eliminates ambiguity, enforces negative constraints (no unrequested refactoring), and defines explicit test/verification criteria. |
| FR-1.3 | Trivial Intent Passthrough Gate: Detect simple commands, stack traces, or direct shell queries that do not benefit from prompt expansion. | P1 | Prompts under a complexity threshold (e.g., "run cargo test") set `passthrough = true` and bypass verbose rewriting to avoid token bloat. |
| FR-1.4 | Repo Symbol Grounding (Fast-Path): Optionally verify file or symbol names mentioned in the raw prompt against the repo file tree and attach exact relative paths. | P2 | If user types "fix auth.rs", the compiler resolves and injects `src/middleware/auth.rs` into `<context>`. |

### 4.3 Module 2: Repository Agent & Skill Discovery Cache

| Req ID | Requirement Description | Priority | Acceptance Criteria |
|---|---|---|---|
| FR-2.1 | Multi-Convention Directory Discovery: Scan standard repository paths (`.agents/`, `.claude/agents/`, `skills/`, `.cursor/rules/`) while respecting `.gitignore`. | P0 | Discovers all `.md` and `.yaml` agent/skill manifests across nested directories. |
| FR-2.2 | Frontmatter & Header Extraction: Parse only the YAML frontmatter (name, description, triggers) or top 15 lines of each manifest so full skill bodies do not bloat the router's prompt. | P0 | Registry index stays under 1,000 tokens even in repositories with 50+ skills and subagents. |
| FR-2.3 | Content-Addressable Mtime Caching: Compute a 64-bit XXH3 hash of all discovered file paths and nanosecond modification timestamps (mtime). | P0 | If no agent/skill files have changed since the last invocation, `.router/registry.json` is loaded from disk in less than 2ms with zero file re-parsing. |

### 4.4 Module 3: Harness-Agnostic Subagent Dispatcher

| Req ID | Requirement Description | Priority | Acceptance Criteria |
|---|---|---|---|
| FR-3.1 | Subagent & Skill Matching: Select the single most qualified `target_subagent` and an array of `required_skills` based on the compiled prompt and cached registry. | P0 | Router outputs valid paths/IDs matching the cached registry; falls back to `default_generalist` if no specialized agent matches. |
| FR-3.2 | Adapter Mode A — CLI Wrapper / Subprocess Spawner: Spawn any target CLI harness (claude, aider, codex, opencode, custom scripts) passing the compiled prompt, target subagent, and session reset flags. | P0 | Properly forwards stdin/stdout/stderr, injects environment variables (`ROUTED_SUBAGENT`, `ROUTED_SKILLS`, `ROUTED_SESSION_RESET`), and propagates exit codes. |
| FR-3.3 | Adapter Mode B — Stdout Hook Contract: Emit a deterministic JSON payload to stdout when no harness binary is specified, enabling integration with pre-prompt hooks. | P0 | Outputs clean, parseable JSON to stdout while routing all diagnostic logs strictly to stderr. |
| FR-3.4 | Adapter Mode C — Native MCP Stdio Server: Run as a long-lived JSON-RPC 2.0 server via `agent-router --mcp`, exposing a `route_and_compile` tool to any MCP client. | P1 | Passes MCP Inspector handshake and tool invocation checks without external runtime dependencies. |

## 5. Non-Functional Requirements (NFRs)

* **Zero External Dependencies (Strict Static Linking):**
  * The compiled artifact must not link against `libssl`, `libcrypto`, `libsqlite3`, or dynamic C++ standard libraries.
  * On Linux, `ldd ./agent-router` must output statically linked or `not a dynamic executable`.
* **Binary Size & Memory Footprint:**
  * Release binary size must remain under 5 MB (stripped with LTO enabled).
  * Peak resident memory (RSS) during execution must remain under 15 MB.
* **Graceful Degradation (Fail-Open Resilience):**
  * If the low-cost LLM endpoint (e.g., local Ollama or cloud API) is unreachable or times out (configurable timeout default: 3,000ms), the binary must fail open: it logs a warning to stderr, performs heuristic keyword matching against the cached registry, enforces the 35% token check locally, and dispatches the raw prompt so developer workflow is never blocked.
* **Security & Credential Hygiene:**
  * API keys (`ROUTER_LLM_KEY`) are read strictly from environment variables or OS keychain paths and are never written to `.router/` state files or command-line process arguments (`ps aux` safe).

## 6. System Architecture & Data Contracts

### 6.1 End-to-End Execution Flow

```
[User Raw Prompt]
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│                agent-router (Static Binary)                 │
│                                                             │
│  ┌───────────────────────────────────────────────────────┐  │
│  │ Step 0: Context Circuit Breaker                       │  │
│  │ • Reads .router/session.json                          │  │
│  │ • Calculates: (Overhead + History + Prompt) / MaxWin  │  │
│  │ • Sets must_reset_session = (Saturation > 0.35)       │  │
│  └──────────────────────────┬────────────────────────────┘  │
│                             ▼                               │
│  ┌───────────────────────────────────────────────────────┐  │
│  │ Step 2: Mtime-Cached Capability Registry              │  │
│  │ • Computes XXH3 hash of .agents/ & skills/ mtimes     │  │
│  │ • Hits .router/registry.json (<2ms) or re-indexes     │  │
│  └──────────────────────────┬────────────────────────────┘  │
│                             ▼                               │
│  ┌───────────────────────────────────────────────────────┐  │
│  │ Step 1: Low-Cost LLM Compiler & Router                │  │
│  │ • Calls Local (Ollama) or Cloud (Flash-Lite/DeepSeek) │  │
│  │ • Compiles XML Prompt + Selects Subagent & Skills     │  │
│  │ • Generates State Handoff Summary if Reset == true    │  │
│  └──────────────────────────┬────────────────────────────┘  │
│                             ▼                               │
│  ┌───────────────────────────────────────────────────────┐  │
│  │ Step 3: Harness-Agnostic Dispatcher                   │  │
│  │ • Updates .router/session.json                        │  │
│  │ • Dispatches via CLI Wrapper, Hook JSON, or MCP Stdio │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────┬───────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
 [CLI Subprocess]     [Pre-Prompt Hook]     [MCP JSON-RPC Client]
 (Claude / Custom)   (Cursor / Windsurf)    (LangGraph / PydanticAI)
```

### 6.2 Core JSON Output Contract

Whether consumed via stdout hook or MCP tool response, the router guarantees the following payload structure:

```json
{
  "session_meta": {
    "session_id": 4,
    "saturation_ratio": 0.382,
    "threshold_exceeded": true,
    "action": "START_NEW_SESSION"
  },
  "routing": {
    "passthrough": false,
    "target_subagent": "rust-systems-architect",
    "target_subagent_path": ".agents/rust-systems-architect.md",
    "required_skills": [
      "skills/zero-dep-cross-compile/SKILL.md"
    ]
  },
  "payload": {
    "context_handoff_summary": "Completed Step 0 token estimator and XXH3 registry cache. Pending: JSON-RPC stdio loop implementation in src/mcp.rs.",
    "compiled_prompt": "<objective>\nImplement the synchronous JSON-RPC 2.0 stdio server loop in `src/mcp.rs`.\n</objective>\n<constraints>\n- Zero async runtime dependencies (use `std::io::BufRead` only).\n- Must handle `initialize`, `tools/list`, and `tools/call` methods.\n</constraints>\n<verification>\nRun `cargo test --test mcp_handshake` and verify zero compiler warnings.\n</verification>"
  }
}
```

## 7. Risk Register & Mitigations

| Risk | Likelihood | Impact | Mitigation Strategy |
|---|---|---|---|
| Cheap model hallucinates non-existent subagent IDs | Medium | High | Validate returned `target_subagent` and `required_skills` against the keys of `RegistryCache` in Rust after deserialization; strip invalid skill IDs and fall back to default agent if mismatched. |
| Token estimation drifts from actual harness token usage | Medium | Medium | Allow external harnesses or post-execution hooks to call `agent-router --sync-tokens <actual_count>` to calibrate the ledger with exact API usage figures after each turn. |
| Cheap model latency slows down interactive CLI usage | Low | Medium | Enforce a strict 3-second HTTP timeout; recommend local quantized 3B/7B models or ultra-low-latency providers (Groq / Gemini Flash-Lite) and bypass compilation when passthrough heuristics match. |
| Aggressive 35% reset drops subtle mid-task nuance | Medium | High | Maintain a rolling summary in `.router/session.json` that explicitly records active file paths, last git diff stat, and unresolved errors so the new session starts with high-density structural context. |

## 8. Phased Delivery & Release Plan

* **Phase 1: Core Binary & Local State Engine (Milestone 1)**
  * Implement CLI argument parser, `.router/` state management, Step 0 token heuristic calculator (35% trigger), and Step 2 xxh3 mtime registry scanner with YAML frontmatter extraction.
* **Phase 2: Prompt Compiler & Fail-Open HTTP Client (Milestone 2)**
  * Implement pure-Rust ureq + rustls OpenAI-compatible client, deterministic meta-prompt templates, JSON schema validation, and fail-open fallback logic.
* **Phase 3: Harness Adapters & MCP Server Mode (Milestone 3)**
  * Implement subprocess wrapper mode (`ROUTER_HARNESS_BIN`), hook stdout mode, and synchronous JSON-RPC 2.0 `--mcp` stdio mode.
* **Phase 4: Zero-Dependency CI/CD Matrix (Milestone 4)**
  * Configure GitHub Actions workflow using `GITHUB_TOKEN` to build, strip, and publish static binaries across Linux (musl), macOS (Universal), and Windows (msvc-static).
