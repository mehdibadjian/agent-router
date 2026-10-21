# Business Requirements Document (BRD)

**Project Name:** AgentRouter (Zero-Dependency System 1 / System 2 Cognitive Pre-Processor & Subagent Dispatcher)
**Document Version:** 2.0 (`v2.0.0`)
**Date:** October 21, 2026
**Target Architecture:** Cargo workspace — reusable `s1s2-core` engine + `ext-agent-router` domain extension + thin static CLI/MCP binary (Rust / Zero Runtime Dependencies)

## 1. Executive Summary & Business Case

Modern AI coding and orchestration harnesses (such as Claude Code, VS Code GitHub Copilot, Cursor, Antigravity, LangGraph, CrewAI, and custom CLI runners) suffer from three compounding operational bottlenecks:

* **Context Window Degradation ("Lost-in-the-Middle"):** As session history grows beyond 35% saturation, frontier "System 2" models experience degraded reasoning accuracy while incurring exponential KV-cache and input token costs.
* **Unstructured User Prompts:** Terse, ambiguous developer prompts force expensive frontier models to spend multiple reasoning turns clarifying intent, grepping files, and guessing verification criteria.
* **Harness Lock-In & Fragmented Discovery:** Repositories increasingly contain specialized subagents, skills, prompts, and instructions (`SKILL.md`, `.agents/*.md`, `.github/agents/*.agent.md`, `.github/prompts/*.prompt.md`, `.github/instructions/*.instructions.md`), but discovering, caching, and routing to them is tightly coupled to specific vendor harnesses or requires heavy runtime dependencies (Python/Node.js).

AgentRouter solves this by introducing a harness-agnostic, zero-dependency native binary that intercepts raw user prompts before they hit expensive execution models. Using a free (local Ollama/MLX) or ultra-low-cost model (Gemini Flash-Lite, DeepSeek, Haiku), it enforces a hard 35% context ceiling, compiles raw prompts into deterministic engineering specifications, caches repository capabilities in sub-milliseconds, and dispatches the payload to the optimal subagent.

**What changed in v2.0.0:** the routing logic above is now the *first extension* of a general engine rather than the architecture itself. The binary is split into a reusable `s1s2-core` crate that owns the System 1 → metacognitive gate → System 2 pipeline, an `ext-agent-router` crate that implements the `CognitiveDomain` trait for developer subagent routing, and a thin `agent-router` binary that presents the result over CLI, lifecycle-hook, subprocess-wrap, and MCP stdio transports. The end-user contract is unchanged: every v0.3.0 flag, stdout envelope, `ROUTED_*` variable, hook JSON payload, and `.router/` state file is still produced exactly as before, and the v2 additions (`--prompt`, `--mcp-stdio`, `--wrap`, the `cognitive` envelope block, `ROUTED_PATHWAY`) are strictly additive.

## 2. Business Objectives & Success Metrics (KPIs)

| Objective Category | Target Metric (KPI) | Baseline (Unrouted Harness) | Target (With AgentRouter) |
|---|---|---|---|
| Cost Efficiency | Average Frontier Token Spend per Task | 100% (Unbounded context growth) | Reduces token spend by 45% to 65% via 35% hard ceiling and prompt compilation |
| Execution Quality | First-Pass Task Completion Rate | ~60% on ambiguous prompts | Greater than 85% via injected XML constraints and verification steps |
| Portability & Ops | Host Runtime Dependencies | Requires Python/uvx, Node/npx, or Docker | Zero dependencies (Single static binary; ldd returns "not a dynamic executable") |
| System Latency | Local Binary Overhead (Excl. LLM network call) | 300ms to 1,500ms (Python/Node cold start) | Less than 10ms cold start + cache validation |
| Context Hygiene | Maximum Context Saturation Allowed | 80% to 95% (Auto-compact triggers late) | Hard limit at 35% of target model's context window |
| Reuse (v2) | Cognitive logic shipped as a reusable crate | 0% (policy welded to the binary) | `s1s2-core` compiles and runs a second domain with zero engine edits (`tests/mock_domain.rs`) |
| Compatibility (v2) | v0.3.0 contract regressions | n/a | 0 — the legacy `tests/pipeline.rs` and `tests/hooks.rs` suites run unmodified against the v2 binary (14 tests) |

## 3. Project Scope

### 3.1 In-Scope

* **Reusable Cognitive Engine (`s1s2-core`):** A domain-agnostic `CognitiveEngine<D>` that runs the strict `budget → System 1 → metacognitive gate → System 2` pipeline over a `CognitiveDomain` trait with five associated types and six static-dispatch methods, plus the shared building blocks any domain needs: `MtimeCache` (xxh64 fingerprint over paths + nanosecond mtimes), `CircuitBreaker` (30% soft warning / 35% hard ceiling), `MetacognitiveGate` (`GateVerdict::{FastCommit, Escalate, Halt}`), `XmlEnvelopeBuilder` (entity-escaped, injection-contained), an optional feature-gated `ureq` adjudicator, and the `render_handoff_artifact` continuity record.
* **Extension #1 — Developer Subagent Routing (`ext-agent-router`):** The existing product, reimplemented as a `CognitiveDomain` implementation: registry discovery, deterministic heuristic scoring, ledger-aware budget measurement, gate policy, prompt compilation, and circuit-trip recovery.
* **Thin Harness Binary (`bins/agent-router`):** Argument dispatch, `.router/` ledger state, and the wire formats for each transport. It contains no cognitive policy of its own.
* **Zero-Dependency Cross-Platform Binary:** Native compilation for Linux (x86_64 / aarch64 static musl), macOS (Universal Apple Silicon + Intel), and Windows (x86_64 static MSVC CRT).
* **Step 0 (Context Circuit Breaker):** Token ledger tracking, live `transcript_path` token measurement, configurable harness overhead accounting, 35% saturation enforcement, multi-turn `rolling_summary` breadcrumb tracking, and `.router/HANDOFF.md` state-handoff persistence across blocked -> fresh session resets.
* **Step 1 (Prompt Compiler):** Integration with any OpenAI-compatible local or cloud endpoint to transform raw user prompts into structured specifications (Objective, Constraints, Context, Verification), plus a passthrough gate for trivial commands, plus a deterministic local compiler used whenever no adjudicator is configured or reachable.
* **Step 2 (Capability Registry & Cache):** High-speed filesystem scanner and frontmatter parser for repo-local agents, skills, prompts, and instructions, backed by xxh64 modification-time (mtime) hashing.
* **Step 3 (Harness-Agnostic Dispatcher):** Multi-mode execution supporting CLI subprocess wrapping (`ROUTER_HARNESS_BIN` and `--wrap`), raw stdout JSON, a native stdin/stdout lifecycle hook adapter (`--hook`) for VS Code Copilot, Claude Code, and Antigravity (AGY), and an MCP stdio server (`--mcp-stdio`) exposing `route_intent`, `check_context_budget`, and `refresh_registry`.

### 3.2 Out-of-Scope

* Building a full "System 2" autonomous coding loop (AgentRouter delegates execution to existing harnesses/subagents).
* GUI or web dashboard interfaces (strictly CLI, environment variables, stdio hook JSON, and MCP stdio JSON-RPC).
* External database dependencies (all state is persisted locally in `.router/*.json` and `.router/HANDOFF.md`).
* Async runtimes, actor frameworks, message brokers, and dynamic trait objects — the engine is statically dispatched and single-threaded per invocation.
* An MCP HTTP/SSE transport or an auth-bearing tool surface (credentials never traverse the protocol).
* A second production domain. `s1s2-core` ships the `mock_domain` acceptance test as the reference extension; no other domain is published in this release.

## 4. Functional Requirements

### 4.1 Module 0: Context Usage Monitor & 35% Circuit Breaker

| Req ID | Requirement Description | Priority | Acceptance Criteria |
|---|---|---|---|
| FR-0.1 | Zero-Dependency Token Estimation & Live Transcript Sizing: Estimate token counts for system overhead, session history (including live `transcript_path` files passed by VS Code / Claude / AGY), injected skills, and the incoming prompt without external C/C++ tokenizers. | P0 | Token count calculation completes in less than 1ms with less than 3% variance against standard BPE (cl100k_base / o200k_base). |
| FR-0.2 | Configurable Harness Overhead: Account for hidden tokens injected by the underlying harness (system prompts, tool definitions, rules files). | P0 | Supports `ROUTER_BASE_OVERHEAD_TOKENS` (default: 10000) and `ROUTER_MAX_TOKENS` (default: 128000). |
| FR-0.3 | 35% Hard Saturation Trigger & Hook Block Pinning: Evaluate total projected tokens against `ROUTER_MAX_TOKENS`. If usage exceeds 0.35 (35%), flag the session for mandatory reset and block execution in `--hook` mode (`continue: false, decision: "block"`), keeping `accumulated_tokens` pinned until the host `sessionId` changes on New Chat (`+`) or `/clear`. | P0 | Any request pushing cumulative session tokens above 35.0% sets `must_reset_session = true` deterministically and continues blocking same-session retries until a fresh session starts. |
| FR-0.4 | Rolling Summary Breadcrumbs & `.router/HANDOFF.md` Persistence: Append turn-by-turn objective breadcrumbs into `rolling_summary` during normal turns; when a reset is triggered, persist the compressed handoff to `.router/HANDOFF.md` and inject `<session_handoff>` on the first non-passthrough turn of the new session. | P0 | New session starts with a clean ledger (`accumulated_tokens = 0`), preserving prior turn breadcrumbs across trivial passthrough commands (`git status`) and deleting `.router/HANDOFF.md` once consumed. |

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
| FR-2.1 | Multi-Convention Directory Discovery: Scan standard repository paths (`.agents/`, `.claude/agents/`, `.github/agents/`, `skills/`, `.claude/skills/`, `.github/skills/`, `.github/prompts/`, `.github/instructions/`). | P0 | Discovers all `.md` agent, skill, prompt, and instruction manifests across nested directories and strips `.agent.md`, `.prompt.md`, and `.instructions.md` double extensions. |
| FR-2.2 | Frontmatter & Header Extraction: Parse only the YAML frontmatter (name, description, triggers) or top lines of each manifest so full skill bodies do not bloat the router's prompt. | P0 | Registry index stays under 1,000 tokens even in repositories with 50+ skills and subagents. |
| FR-2.3 | Content-Addressable Mtime Caching: Compute a 64-bit xxh64 hash over the sorted `path:mtime_nanos:size` fingerprint of all discovered file paths (v0.3.0 used xxh3; the v2 dependency allowlist pins `xxh64`, and the on-disk `{hash, entries}` schema is unchanged). | P0 | If no agent/skill files have changed since the last invocation, `.router/registry.json` is loaded from disk in less than 2ms with zero file re-parsing. |

### 4.4 Module 3: Harness-Agnostic Subagent Dispatcher

| Req ID | Requirement Description | Priority | Acceptance Criteria |
|---|---|---|---|
| FR-3.1 | Subagent & Skill Matching: Select the single most qualified `target_subagent` and an array of `required_skills` based on the compiled prompt and cached registry. | P0 | Router outputs valid paths/IDs matching the cached registry; falls back to `default` if no specialized agent matches or in skills-only repositories. |
| FR-3.2 | Adapter Mode A — CLI Wrapper / Subprocess Spawner: Spawn any target CLI harness (claude, agy, aider, codex, opencode, custom scripts) passing the compiled prompt, target subagent, and session reset flags. | P0 | Properly forwards stdin/stdout/stderr, injects environment variables (`ROUTED_SUBAGENT`, `ROUTED_SKILLS`, `ROUTED_SESSION_ACTION`), and propagates exit codes. |
| FR-3.3 | Adapter Mode B — Stdout JSON Contract: Emit a deterministic JSON payload to stdout when invoked with a prompt argument and no harness binary, enabling custom scripts. | P0 | Outputs clean, parseable JSON to stdout while routing all diagnostic logs strictly to stderr. |
| FR-3.4 | Adapter Mode C — Native Lifecycle Hook Adapter: Run via `agent-router --hook`, reading hook stdin JSON and outputting harness-native context injection (`hookSpecificOutput.additionalContext` with `<routing_directive>` + `systemMessage` status banner for VS Code Copilot & Claude Code; `injectSteps` for Antigravity). | P0 | Zero-config drop-in hook for `.github/hooks/*.json`, `.claude/settings.json`, and `.agents/hooks.json`; enforces 35% hard blocks (`continue: false, decision: "block"`) and fails open with `{}` on malformed input. |
| FR-3.5 | Adapter Mode D — MCP stdio Server (v2): Run via `agent-router --mcp-stdio`, speaking newline-delimited JSON-RPC 2.0 over stdin/stdout with `initialize`, `tools/list`, and `tools/call` for `route_intent`, `check_context_budget`, and `refresh_registry`. | P0 | A request without `id` is a notification and receives no reply. Tool-handler failure returns `result.isError = true` content, not a JSON-RPC error; `-32700`/`-32601`/`-32602` are reserved for parse, unknown-method, and unknown-tool faults. `route_intent` output is byte-identical to the `--prompt` envelope. |
| FR-3.6 | Explicit Prompt and Wrap Entry Points (v2): `--prompt [text]` (stdin when the text is omitted) and `--wrap <CMD> [args...]` provide the same turn through flags rather than a positional join or the `ROUTER_HARNESS_BIN` environment variable. | P1 | `--wrap` suppresses stdout, injects the full `ROUTED_*` block, and propagates the child exit code unchanged. The legacy positional form remains accepted. |

### 4.5 Module 4: Reusable Cognitive Engine (v2.0.0)

| Req ID | Requirement Description | Priority | Acceptance Criteria |
|---|---|---|---|
| FR-4.1 | `CognitiveDomain` Trait: A single trait with five associated types (`Input`, `RegistryState`, `FastOutput`, `FinalDecision`, `Config`) and six required methods (`build_registry`, `measure_budget`, `execute_system1`, `evaluate_gate`, `execute_system2`, `handle_circuit_trip`) plus defaulted hooks (`domain_id`, `watched_dirs`, `cache_file`, `registry_extensions`, `cache_payload_key`, `annotate_pathway`). | P0 | Static dispatch only: no `dyn`, no vtable, no `async`. A second domain compiles and passes the full pipeline in `crates/s1s2-core/tests/mock_domain.rs` without touching the engine. |
| FR-4.2 | Metacognitive Gate: A pure decision function over `(force_system2, confidence, candidate_count, saturation_ratio)` returning `GateVerdict::FastCommit`, `GateVerdict::Escalate { partial, reason }`, or `GateVerdict::Halt(trip)`. | P0 | Evaluation order is explicit: `force_compile` → `ExplicitSystem2Request`; `ratio >= 0.30` → `ContextSaturationWarning`; `confidence < 0.85` → `LowConfidence`; `candidate_count != 1` → `AmbiguousCandidates`; else commit. A domain can additionally coerce a halt above the hard ceiling, and the engine independently checks it before System 1 runs. |
| FR-4.3 | System 1 Fast Path: Deterministic heuristics with no I/O, no clock, and no network. | P0 | `engine.process()` on a warm cache commits in under 5 ms measured by the engine's own timer and asserted in `ext-agent-router/tests/pathways.rs`. |
| FR-4.4 | System 2 Deliberation: XML envelope compilation, optional synchronous adjudication, capability-matrix tie-breaks, and hallucinated-ID sanitization. | P0 | `ureq` sits behind the `llm` feature; `cargo tree -p s1s2-core` shows zero `ureq` nodes with default features. Adjudicator failure logs to stderr and falls open to the deterministic compiler, which is byte-stable across runs. |
| FR-4.5 | Circuit-Trip Handoff Artifact: On halt, emit a machine-readable artifact carrying the ledger (session id, turn count, accumulated tokens, every breadcrumb, last pathway) plus the ceiling that was breached. | P0 | The artifact is written to `.router/HANDOFF.md` on the blocked turn, consumed on the first non-passthrough turn of the next session, and injected as `<session_handoff>`; the legacy summary text stays identical. |
| FR-4.6 | Pathway Observability: Every result is wrapped in `CognitiveEnvelope { pathway, saturation_ratio, latency_micros, cache_hit, decision }`, and the pathway is also stamped onto the decision. | P1 | The harness surfaces it as the additive `cognitive` object in the stdout envelope and as `ROUTED_PATHWAY`; legacy consumers that ignore both are unaffected. |
| FR-4.7 | Backward-Compatible Cache Schema: `MtimeCache` writes the legacy `{hash, <payload_key>}` document, with `cache_payload_key()` returning `entries` for the routing domain. | P0 | A `.router/registry.json` written by v0.3.0 is read by v2.0.0 without re-indexing, and the v2 file remains readable by a v0.3.0 checkout. |

## 5. Non-Functional Requirements (NFRs)

* **Zero External Dependencies (Strict Static Linking):**
  * The compiled artifact must not link against `libssl`, `libcrypto`, `libsqlite3`, or dynamic C++ standard libraries.
  * On Linux, `ldd ./agent-router` must output statically linked or `not a dynamic executable`.
* **Binary Size & Memory Footprint:**
  * Release binary size must remain under 5 MB (stripped with LTO enabled).
  * Peak resident memory (RSS) during execution must remain under 15 MB.
* **Graceful Degradation (Fail-Open Resilience):**
  * If the low-cost LLM endpoint (e.g., local Ollama or cloud API) is unreachable or times out (configurable timeout default: 3,000ms), the binary must fail open: it logs a warning to stderr, performs heuristic keyword matching against the cached registry, enforces the 35% token check locally, and dispatches the prompt so developer workflow is never blocked.
* **Security & Credential Hygiene:**
  * API keys (`ROUTER_LLM_KEY`) are read strictly from environment variables or OS keychain paths and are never written to `.router/` state files or command-line process arguments (`ps aux` safe).
* **Fast-Path Latency (v2):**
  * A System 1 turn on a warm registry cache must complete in under 5 ms with zero network and zero file re-parsing; the engine times itself and the extension asserts the bound in `ext-agent-router/tests/pathways.rs`.
* **Dependency Allowlist (v2):**
  * Only `serde`, `serde_json`, `xxhash-rust` (xxh64), `walkdir`, and `ureq` (optional, feature-gated) may appear in the workspace graph. No async runtime, no tokenizer or TLS C library, no CLI framework, no `dyn` dispatch in the engine.
* **Engine Isolation (v2):**
  * `s1s2-core` must compile and pass its own suite with the `llm` feature off, and `cargo tree -p s1s2-core` must contain zero `ureq` nodes by default.
* **Backward Compatibility (v2):**
  * The v0.3.0 test suites (`tests/pipeline.rs`, `tests/hooks.rs`) are the acceptance gate for the refactor and must pass unmodified; `.router/registry.json` keeps the `{hash, entries}` schema and `.router/session.json` only gains fields with serde defaults.
* **Zero-Warning Build (v2):**
  * `cargo build --workspace --all-targets` must emit no warnings.

## 6. System Architecture & Data Contracts

### 6.1 End-to-End Execution Flow

```
[User Raw Prompt / Hook Stdin / MCP tools/call]
       │
       ▼
┌───────────────────────────────────────────────────────────────┐
│        agent-router — thin harness (bins/agent-router)        │
│                                                               │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │ Harness State  (bins/agent-router/src/pipeline.rs)      │  │
│  │ • Loads .router/session.json; new host id resets it     │  │
│  │ • Adopts live transcript_path token counts              │  │
│  └────────────────────────────┬────────────────────────────┘  │
│                               ▼                               │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │ CognitiveEngine<AgentRoutingDomain>  (s1s2-core)        │  │
│  │ • Step 0 breaker: (overhead+history+prompt) / window    │  │
│  │ • Step 2 cache: xxh64 over .agents/ & skills/ mtimes    │  │
│  │ • Step 1 System 1: deterministic, no I/O, <5ms          │  │
│  │ • Gate: FastCommit | Escalate(reason) | Halt(trip)      │  │
│  │ • System 2: XML ± feature-gated ureq adjudicator        │  │
│  └────────────────────────────┬────────────────────────────┘  │
│                               ▼                               │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │ Dispatch  (envelope.rs · hook.rs · mcp.rs)              │  │
│  │ • Commits the ledger; HANDOFF.md written on a halt      │  │
│  │ • CLI wrapper, stdout JSON, --hook, or MCP reply        │  │
│  └─────────────────────────────────────────────────────────┘  │
└───────────────────────────────┬───────────────────────────────┘
                                │
                ┌───────────────┼───────────────┐
                ▼               ▼               ▼
   [CLI Subprocess]    [Stdout Envelope]   [Lifecycle Hooks]
     (Mode A)             (Mode B)         (Mode C: --hook)

Mode D (--mcp-stdio) is the same envelope over newline-delimited JSON-RPC 2.0:
route_intent commits a turn, check_context_budget probes without committing,
refresh_registry rebuilds .router/registry.json from disk.
```

### 6.2 Core JSON Output Contract

When consumed via stdout (`agent-router "<prompt>"`), the router guarantees the following payload structure (which `--hook` formats into `hookSpecificOutput.additionalContext` or `injectSteps`):

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
    "target_subagent": "default",
    "target_subagent_path": "default",
    "required_skills": []
  },
  "payload": {
    "context_handoff_summary": "Prior session reset at >35% context ceiling. Previous summary: design ast node structs | wire up parser errors",
    "compiled_prompt": "<circuit_breaker_tripped max=\"0.35\" saturation=\"0.382\" action=\"START_NEW_SESSION\">\n  <compaction_directive>Context ceiling exceeded. Stop expanding, compress the working state into the handoff summary, and resume in a fresh session.</compaction_directive>\n  <objective>Implement the lifecycle hook adapter</objective>\n</circuit_breaker_tripped>"
  },
  "cognitive": {
    "pathway": "circuit_breaker_halt:saturation-exceeded:0.382>0.350",
    "escalation": "saturation-exceeded:0.382>0.350",
    "latency_micros": 412,
    "cache_hit": true
  }
}
```

`session_meta`, `routing`, and `payload` are the v0.3.0 contract and are emitted field-for-field as before. `cognitive` is the v2.0.0 addition: `pathway` is `system1_fast`, `system2_deliberative:<reason>`, or `circuit_breaker_halt:<trip>`; `escalation` is `null` on the fast path; `latency_micros` and `cache_hit` come straight from `CognitiveEnvelope`. Consumers that ignore unknown keys are unaffected, and the same object is exported to a wrapped harness as `ROUTED_PATHWAY`.

## 7. Risk Register & Mitigations

| Risk | Likelihood | Impact | Mitigation Strategy |
|---|---|---|---|
| Cheap model hallucinates non-existent subagent IDs | Medium | High | Validate returned `target_subagent` and `required_skills` against the keys of `RegistryCache` in Rust after deserialization; strip invalid skill IDs and fall back to default agent if mismatched. |
| Token estimation drifts from actual harness token usage | Medium | Medium | Allow external harnesses or post-execution hooks to call `agent-router --sync-tokens <actual_count>` to calibrate the ledger with exact API usage figures after each turn. |
| Cheap model latency slows down interactive CLI usage | Low | Medium | Enforce a strict 3-second HTTP timeout; recommend local quantized 3B/7B models or ultra-low-latency providers (Groq / Gemini Flash-Lite) and bypass compilation when passthrough heuristics match. |
| Aggressive 35% reset drops subtle mid-task nuance | Medium | High | Maintain a rolling summary in `.router/session.json` that explicitly records active file paths, last git diff stat, and unresolved errors so the new session starts with high-density structural context. |

## 8. Phased Delivery & Release Plan

* **Phase 1: Core Binary & Local State Engine (Milestone 1)**
  * Implement CLI argument parser, `.router/` state management, Step 0 token heuristic calculator (35% trigger), and Step 2 xxh64 mtime registry scanner with YAML frontmatter extraction.
* **Phase 2: Prompt Compiler & Fail-Open HTTP Client (Milestone 2)**
  * Implement pure-Rust ureq + rustls OpenAI-compatible client, deterministic meta-prompt templates, JSON schema validation, and fail-open fallback logic.
* **Phase 3: Harness Adapters & Native Hook Mode (Milestone 3)**
  * Implement subprocess wrapper mode (`ROUTER_HARNESS_BIN`), raw stdout envelope mode, and native lifecycle hook mode (`--hook`) for VS Code Copilot, Claude Code, and Antigravity.
* **Phase 4: Zero-Dependency CI/CD Matrix (Milestone 4)**
  * Configure GitHub Actions workflow using `GITHUB_TOKEN` to build, strip, and publish static binaries across Linux (musl), macOS (Universal), and Windows (msvc-static).

## 9. v2.0.0 Migration Record (delivered)

| Phase | Work | Gate |
|---|---|---|
| M1 — Engine extraction | New Cargo workspace (virtual root manifest + `[workspace.dependencies]` allowlist). `crates/s1s2-core` ships `domain.rs` (the `CognitiveDomain` contract), `gate.rs` (`MetacognitiveGate`), `s1/circuit.rs` (breaker + token estimate), `s1/cache.rs` (`MtimeCache`), `s2/` (XML compiler, feature-gated `llm` client, handoff artifact), `harness/` (CLI + MCP adapters), and `CognitiveEngine<D>`. | `cargo test -p s1s2-core` (36 tests, `llm` off); `cargo tree -p s1s2-core` shows 0 `ureq` nodes. |
| M2 — Domain port | `crates/ext-agent-router` holds the routing logic as `impl CognitiveDomain for AgentRoutingDomain` over the existing `registry`/`heuristics`/`compiler`/`types` modules; the legacy `src/{budget,compiler,dispatcher,registry,types}.rs` files are deleted. | 24 crate tests + the 4 spec-mandated acceptance tests in `tests/pathways.rs` (fast commit <5 ms, escalate + deterministic recompilation, 37.5% halt with handoff artifact, mtime invalidation with the legacy `{hash, entries}` schema). |
| M3 — Thin harness | `bins/agent-router`: `main.rs` flag dispatch, `pipeline.rs` ledger orchestration, `envelope.rs` wire format, `hook.rs` rewritten on top of `pipeline`, `mcp.rs` tool adapters, `tests/harness.rs` for the v2 surface. | Legacy `tests/pipeline.rs` (8) and `tests/hooks.rs` (6) pass unmodified; 8 new harness tests; 1 binary unit test. |
| M4 — Release plumbing | Docs rewritten for the workspace topology; `[workspace.package].version` becomes the single version literal (members inherit `version.workspace = true`); release-please moved off the `rust` release type (which requires a `[package]` manifest at the component root) onto a `simple` component plus a TOML `extra-files` updater on `$.workspace.package.version`, with a one-shot `release-as: 2.0.0` over the still-`0.3.0` manifest; CI switched to `cargo test --workspace --all-targets` with an added default-feature `ureq`-absence assertion. | 83 tests green, zero build warnings, 1.82 MiB release binary. |

Deliberate deviations, recorded for future readers:

* **`xxh64`, not `xxh3`.** The v2 dependency allowlist pins the `xxh64` feature of `xxhash-rust`. Only the fingerprint function changed; the on-disk `{hash, entries}` document is byte-compatible with v0.3.0, so a warm cache survives the upgrade.
* **No new environment variables.** The 30% warning and 35% ceiling come from core constants (`DEFAULT_SOFT_WARNING_RATIO`, `DEFAULT_HARD_CEILING_RATIO`) rather than host config, so a misconfigured machine cannot silently raise the ceiling. Every v0.3.0 variable keeps its old meaning.
* **`evaluate_gate` receives `&CircuitBreaker`.** The spec's six-method contract is unchanged in name and order; passing the breaker lets a domain consult the hard limit without re-deriving it, which is what lets Extension #1 defend the ceiling in both the engine and its own gate.
* **One version literal.** Member crates declare `version.workspace = true`, so a release bumps exactly one field. Path dependencies in `[workspace.dependencies]` carry no `version`, which keeps `cargo publish`-style metadata out of an unpublished private workspace and removes a second place for the version to drift.
* **Release type `simple`, not `rust`.** release-please's `rust` release type rewrites `[package].version` in the component-root manifest and errors on a virtual workspace manifest. The single root component therefore uses the `simple` type with an explicit TOML extra-file updater on `$.workspace.package.version`, which keeps one tag (`v2.0.0`), one root `CHANGELOG.md`, and one version literal.
* **`release-as: "2.0.0"` is a one-shot override.** `.release-please-manifest.json` still records `0.3.0`, the last version actually tagged, so the next release-please run opens the `v2.0.0` Release PR (with the `feat!` breaking-change note from this migration) instead of skipping straight to `2.0.1`. The key must be deleted once `v2.0.0` is cut, after which normal semver bumps resume.
* **`session.json` gains fields only with serde defaults** (`turn_count`, `last_pathway`), so a ledger written by v0.3.0 loads unchanged and a v2 ledger is still readable by older checkouts.
