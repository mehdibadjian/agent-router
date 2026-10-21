# Zero-Hallucination Implementation Plan

**Project:** AgentRouter (Zero-Dependency System 1 / System 2 Cognitive Engine + Subagent Dispatcher)
**Document Version:** 2.0 (`v2.0.0`)
**Date:** October 21, 2026
**Companion Document:** [BRD.md](./BRD.md)

This Zero-Hallucination Implementation Plan is structured as an execution-ready blueprint you can hand directly to a coding agent (or follow file-by-file). It locks down exact crate versions, prohibits unrequested dependencies (no tokio, no openssl, no clap, no serde_yaml), defines every struct and function signature, and includes deterministic unit tests for every module.

> **Reading order for v2.0.0:** §1–§4 and §6 describe the current workspace and are authoritative. §5 preserves the v0.3.0 single-package file-by-file specification verbatim as the historical record — its module paths (`src/budget.rs`, `src/dispatcher.rs`, …) no longer exist, and §6 maps each of those files to the crate that absorbed it.

## 1. Anti-Hallucination Guardrails (Agent Constraints)

Any coding agent executing this specification MUST obey these hard rules:

* **Strict Crate Lock:** Only the 5 crates in `[workspace.dependencies]` are permitted (`serde`, `serde_json`, `ureq`, `xxhash-rust`, `walkdir`). Do not add `clap`, `tokio`, `reqwest`, `anyhow`, `regex`, or `serde_yaml`. `xxhash-rust` is enabled with the `xxh64` feature only.
* **Synchronous I/O Only:** All HTTP, filesystem, and stdin/stdout hook JSON operations must use blocking standard library I/O (`std::io`, `std::fs`, `ureq`).
* **Pure-Rust TLS, Feature-Gated:** `ureq` v2.10 must be configured with `default-features = false` and `features = ["tls", "json"]` so it compiles `rustls` statically without linking system OpenSSL. In `s1s2-core` it is `optional = true` behind the `llm` feature, so the engine's default graph contains zero network code.
* **Static Dispatch Only:** Domains plug in through the `CognitiveDomain` trait and generic `CognitiveEngine<D>`. No `dyn Trait`, no vtables, no `async`, no executor.
* **Stdout Purity:** Never use `println!` for logging or diagnostics—stdout is strictly reserved for the JSON routing contract, lifecycle hook responses, and MCP JSON-RPC framing. All warnings/logs must use `eprintln!`.
* **Fail-Open Mandate:** Network timeouts, malformed LLM JSON, or missing API keys must never panic or crash the binary; they must fall back deterministically to the local compiler.
* **Contract Freeze:** `bins/agent-router/tests/pipeline.rs` and `bins/agent-router/tests/hooks.rs` are the v0.3.0 wire contract. They may not be edited to accommodate a refactor; the refactor must satisfy them as written.

## 2. Repository Directory Layout

```
agent-router/
├── .cargo/
│   └── config.toml              # Static linking flags for Linux musl & Windows MSVC
├── .github/
│   ├── hooks/
│   │   └── agent-router.json    # VS Code Copilot UserPromptSubmit hook config
│   └── workflows/ci.yml         # workspace tests + 4-platform static matrix
├── .vscode/
│   └── settings.json            # Enables chat.useHooks and customAgentInSubagent
├── bins/
│   └── agent-router/            # Thin CLI / hook / MCP binary (no cognitive policy)
│       ├── Cargo.toml
│       ├── src/
│       │   ├── main.rs          # std::env::args dispatcher: --prompt/--hook/--mcp-stdio/--wrap/--sync-tokens
│       │   ├── pipeline.rs      # Ledger state in, CognitiveEnvelope out; .router/ persistence
│       │   ├── envelope.rs      # v0.3.0 stdout contract + additive `cognitive` block + ROUTED_* map
│       │   ├── hook.rs          # Mode C: Copilot / Claude / Antigravity lifecycle hooks
│       │   └── mcp.rs           # Mode D: MCP tool adapters over the stdio JSON-RPC server
│       └── tests/
│           ├── common/mod.rs    # Legacy temp-repo fixture (unchanged)
│           ├── pipeline.rs      # v0.3.0 CLI contract suite — MUST pass unmodified
│           ├── hooks.rs         # v0.3.0 hook contract suite — MUST pass unmodified
│           └── harness.rs       # v2.0.0 CLI + MCP surface
├── crates/
│   ├── s1s2-core/               # Reusable engine: zero domain knowledge, network-free by default
│   │   ├── Cargo.toml           # ureq is optional, behind the `llm` feature
│   │   ├── src/
│   │   │   ├── lib.rs           # CognitiveEngine<D> coordinator + CognitiveEnvelope + ExecutionPathway
│   │   │   ├── domain.rs        # CognitiveDomain trait, GateVerdict, EscalationReason, CircuitTrip
│   │   │   ├── gate.rs          # MetacognitiveGate pure decision function
│   │   │   ├── s1/cache.rs      # MtimeCache: xxh64 over sorted path:mtime_nanos:size lines
│   │   │   ├── s1/circuit.rs    # CircuitBreaker, token estimate, 0.30 warn / 0.35 ceiling
│   │   │   ├── s2/xml_compiler.rs # XmlEnvelopeBuilder with entity escaping + attr sanitizing
│   │   │   ├── s2/llm.rs        # feature-gated OpenAI-compatible chat completion client
│   │   │   ├── s2/handoff.rs    # SessionLedger, commit_turn, render_handoff_artifact
│   │   │   └── harness/
│   │   │       ├── cli.rs       # SpawnRequest / spawn (exit-code propagation) / emit_json
│   │   │       └── mcp.rs       # McpServer: newline-delimited JSON-RPC 2.0 over stdio
│   │   └── tests/mock_domain.rs # Acceptance: a second domain plugs in without engine edits
│   └── ext-agent-router/        # Extension #1: developer subagent routing domain
│       ├── Cargo.toml
│       ├── src/
│       │   ├── lib.rs           # impl CognitiveDomain for AgentRoutingDomain + gate/s2 policy
│       │   ├── types.rs         # PromptInput, RouterConfig, RoutingDecision, AgentRegistry
│       │   ├── registry.rs      # Directory discovery + hand-rolled frontmatter parser
│       │   ├── heuristics.rs    # Deterministic lexical scoring, trivial-passthrough gate
│       │   └── compiler.rs      # Deterministic templates, XML envelope, adjudication
│       └── tests/pathways.rs    # The 5 spec-mandated acceptance scenarios
├── docs/                        # BRD.md, IMPLEMENTATION_PLAN.md
├── Cargo.toml                   # Virtual workspace manifest + dependency allowlist
└── Cargo.lock                   # Pinned transitive set (rustls + ring, no OpenSSL)
```

## 3. Configuration & Build Manifests

### File 1: `.cargo/config.toml`

```toml
[target.x86_64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]

[target.aarch64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]

[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```

### File 2: `Cargo.toml` (virtual workspace root)

The root manifest owns three things and nothing else: the member list, the dependency
allowlist, and the size-optimized release profile. `[workspace.package].version` is the
**single** version literal in the repository — release tooling bumps that one field and
every member inherits it.

```toml
[workspace]
resolver = "2"
members = [
    "crates/s1s2-core",
    "crates/ext-agent-router",
    "bins/agent-router",
]

[workspace.package]
version = "2.0.0"
edition = "2021"
repository = "https://github.com/mehdibadjian/agent-router"

# Allowed dependency set (spec §2): no async runtime, no OpenSSL/C bindings.
[workspace.dependencies]
serde = { version = "1.0.210", features = ["derive"] }
serde_json = "1.0.128"
xxhash-rust = { version = "0.8.12", features = ["xxh64"] }
walkdir = "2.5.0"
ureq = { version = "2.10.1", default-features = false, features = ["tls", "json"] }

# Workspace-internal path dependencies carry no version: the release tooling bumps a
# single field, `[workspace.package].version`, and every member inherits it.
s1s2-core = { path = "crates/s1s2-core" }
ext-agent-router = { path = "crates/ext-agent-router" }

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

### File 3: `crates/s1s2-core/Cargo.toml`

```toml
[package]
name = "s1s2-core"
version.workspace = true
edition.workspace = true
repository.workspace = true
description = "Reusable zero-dependency System 1 / System 2 cognitive engine"

[features]
default = []
# System 2 external LLM adjudication via ureq. Off by default so the core
# engine stays network-free and binary-size-neutral for extensions that
# never escalate to a remote adjudicator.
llm = ["dep:ureq"]

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
xxhash-rust = { workspace = true }
walkdir = { workspace = true }
ureq = { workspace = true, optional = true }
```

### File 4: `crates/ext-agent-router/Cargo.toml`

```toml
[package]
name = "ext-agent-router"
version.workspace = true
edition.workspace = true
repository.workspace = true
description = "Extension #1 for s1s2-core: developer subagent routing domain"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
walkdir = { workspace = true }
s1s2-core = { workspace = true, features = ["llm"] }
```

### File 5: `bins/agent-router/Cargo.toml`

```toml
[package]
name = "agent-router"
version.workspace = true
edition.workspace = true
repository.workspace = true
description = "System 1 / System 2 cognitive router CLI and MCP stdio server"

[[bin]]
name = "agent-router"
path = "src/main.rs"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
s1s2-core = { workspace = true }
ext-agent-router = { workspace = true }
```

## 4. v2.0.0 Module Contracts

### 4.1 `s1s2-core::domain` — the plug-in surface

Five associated types and six required methods; everything else is a defaulted hook so a
new domain can start from the trait alone.

```rust
pub trait CognitiveDomain: Sized + Send + Sync + 'static {
    type Input: fmt::Debug + Clone + DeserializeOwned + Serialize;
    type RegistryState: fmt::Debug + Clone + Serialize + DeserializeOwned + Default;
    type FastOutput: fmt::Debug + Clone + Serialize;
    type FinalDecision: fmt::Debug + Clone + Serialize + DeserializeOwned;
    type Config: fmt::Debug + Clone;

    fn build_registry(config: &Self::Config) -> Result<Self::RegistryState, String>;
    fn measure_budget(input: &Self::Input, config: &Self::Config) -> (usize, usize);
    fn execute_system1(input: &Self::Input, registry: &Self::RegistryState, config: &Self::Config)
        -> Result<Self::FastOutput, String>;
    fn evaluate_gate(input: &Self::Input, s1_out: Self::FastOutput, saturation_ratio: f32,
                     config: &Self::Config, breaker: &CircuitBreaker)
        -> GateVerdict<Self::FinalDecision>;
    fn execute_system2(input: &Self::Input, registry: &Self::RegistryState,
                      partial_decision: Option<Self::FinalDecision>, reason: &EscalationReason,
                      config: &Self::Config) -> Result<Self::FinalDecision, String>;
    fn handle_circuit_trip(input: &Self::Input, trip: CircuitTrip, config: &Self::Config)
        -> Result<Self::FinalDecision, String>;

    // Defaults: annotate_pathway, domain_id, watched_dirs, cache_file,
    //           registry_extensions() == ["md"], cache_payload_key() == "payload".
}
```

```rust
pub enum GateVerdict<FastOut> {
    FastCommit(FastOut),
    Escalate { partial: Option<FastOut>, reason: EscalationReason },
    Halt(CircuitTrip),
}

pub enum ExecutionPathway { System1Fast, System2Escalated(EscalationReason), CircuitHalted(CircuitTrip) }

pub struct CognitiveEnvelope<Out> {
    pub pathway: ExecutionPathway,
    pub saturation_ratio: f32,
    pub latency_micros: u128,
    pub cache_hit: bool,
    pub decision: Out,
}
```

`EscalationReason` and `CircuitTrip` implement `Display` as machine-readable slugs
(`low-confidence:0.600/0.850`, `saturation-exceeded:0.375>0.350`) that are reused verbatim
in the `cognitive.pathway` envelope field and in `ROUTED_PATHWAY`.

### 4.2 `s1s2-core` — engine and subsystems

| File | Contract |
|---|---|
| `lib.rs` | `CognitiveEngine::<D>::new(config, CircuitBreakerConfig)`; `process(&mut, &Input) -> Result<CognitiveEnvelope<FinalDecision>, String>`; `budget_check` (no commit), `refresh_registry` (invalidate), `rebuild_registry` (invalidate + rebuild). Order is fixed: cache → budget → breaker trip check → System 1 → gate → System 2/halt, with `D::annotate_pathway` applied on every exit. |
| `gate.rs` | `MetacognitiveGate::new(confidence_threshold, soft_warning_ratio).evaluate(SwitchSignals) -> SwitchOutcome`; order `force_system2` → saturation → confidence → candidate count → commit. |
| `s1/circuit.rs` | `estimate_tokens = ceil(chars/3.6) + (punct_or_newline)/3` (byte-identical to v0.3.0); `DEFAULT_SOFT_WARNING_RATIO = 0.30`, `DEFAULT_HARD_CEILING_RATIO = 0.35`, `DEFAULT_CONTEXT_WINDOW = 200_000`; `round_ratio(r) = (r*1000).round()/1000`; `CircuitBreaker::{read, trip, config}`. |
| `s1/cache.rs` | `MtimeCache<T>::load_or_build(&[PathBuf], FnOnce(&[PathBuf]) -> Result<T, String>) -> Result<CacheState<T>, String>`; fingerprint = `xxh64` (seed 0) over sorted `path:mtime_nanos:size\|` lines filtered by `registry_extensions`; document `{"hash": n, "<payload_key>": T}`. |
| `s2/xml_compiler.rs` | `XmlEnvelopeBuilder::{new, open, text_tag, close, finish}`; body entities escaped, attribute values sanitized, invalid tag names fenced. Untrusted text can never close an envelope it does not own. |
| `s2/llm.rs` | `chat_completion(url, key, model, timeout_ms, system, user) -> Result<Value, String>` and `assistant_content(&Value) -> Result<&str, String>`; compiled only with `--features llm`. |
| `s2/handoff.rs` | `SessionLedger` (adds `turn_count`, `last_pathway` with serde defaults), `commit_turn(...)`, `load_ledger`/`save_ledger`, `render_handoff_artifact(&ledger, &HandoffCause, ratio, max) -> String`, `PathwayKind::{legacy_action, slug}`. |
| `harness/cli.rs` | `SpawnRequest::{new, arg, env}`; `spawn(&SpawnRequest) -> Result<i32, String>` propagating the child exit code; `emit_json` / `to_json_string`. |
| `harness/mcp.rs` | `McpServer::new(name, version).tool(McpTool::new(name, description, schema, handler)).serve_stdio()`; `handle_line` is pure and testable. Notification (no `id`) ⇒ no output; handler `Err` ⇒ `result.isError = true`. |

### 4.3 `ext-agent-router` — Extension #1

| Item | Value |
|---|---|
| Associated types | `Input = PromptInput`, `RegistryState = AgentRegistry`, `FastOutput = HeuristicCandidate`, `FinalDecision = RoutingDecision`, `Config = RouterConfig` |
| Hooks overridden | `domain_id() = "agent-router"`, `cache_file() = .router/registry.json`, `cache_payload_key() = "entries"`, `registry_extensions() = ["md"]`, `watched_dirs() = config.search_dirs`, `annotate_pathway() = decision.pathway_metadata` |
| Gate policy | ceiling re-checked in-domain (defence in depth) → trivial passthrough → `MetacognitiveGate::new(0.85, soft_warning_ratio)` |
| System 2 | `compiler::adjudicate` when `config.llm.enabled`, else/failed → `deterministic()` (XML envelope + capability-matrix tie-break) |
| Halt | `handle_circuit_trip` emits `<circuit_breaker_tripped max="0.35" saturation="…" action="START_NEW_SESSION">` with a `<compaction_directive>`, the legacy summary line, and `render_handoff_artifact` |

### 4.4 `bins/agent-router` — harness only

`pipeline::run_turn(repo_root, raw_prompt, observed_transcript_tokens, host_session_id, force_compile)`
is the single entry point shared by the CLI, the hook, and MCP. It owns ledger load/reset,
`.router/HANDOFF.md` write-on-block / consume-on-first-non-passthrough-turn, the pin-on-block
rule for hook sessions, `commit_turn`, and the stderr diagnostics line. It never scores,
selects, or compiles.

## 5. Step-by-Step Build & Verification Sequence (v2.0.0)

1. `cargo build --workspace --all-targets` — must emit **zero warnings**.
2. `cargo test --workspace --all-targets` — 83 tests: 36 `s1s2-core` (35 unit + `mock_domain`),
   24 `ext-agent-router` (20 unit + 4 pathways), 23 `agent-router` (1 unit + 8 harness + 6 hooks + 8 pipeline).
3. `cargo tree -p s1s2-core | grep -c ureq` — must print `0`; `--features llm` makes it `1`.
4. `cargo build --release` then `ls -l target/release/agent-router` — the artifact path is
   unchanged by the workspace layout, so `.github/hooks/agent-router.json` and the CI
   bundling steps need no edits.
5. Backward-compatibility probe: run the legacy suites alone
   (`cargo test -p agent-router --test pipeline --test hooks`) before touching any harness code.
6. v2 surface probe: `printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize"}' | ./target/release/agent-router --mcp-stdio`.

Legacy → v2 file mapping (for readers of §6):

| v0.3.0 | v2.0.0 owner |
|---|---|
| `src/types.rs` | `crates/ext-agent-router/src/types.rs` (+ `s1s2-core::s2::handoff::SessionLedger` for the ledger) |
| `src/budget.rs` | `crates/s1s2-core/src/s1/circuit.rs` (estimator + breaker) and `ext-agent-router::AgentRoutingDomain::measure_budget` |
| `src/registry.rs` | `crates/s1s2-core/src/s1/cache.rs` (fingerprint/persist) + `crates/ext-agent-router/src/registry.rs` (discovery + frontmatter) |
| `src/compiler.rs` | `crates/s1s2-core/src/s2/{xml_compiler,llm}.rs` + `crates/ext-agent-router/src/{heuristics,compiler}.rs` |
| `src/dispatcher.rs` | `crates/s1s2-core/src/harness/cli.rs` + `bins/agent-router/src/envelope.rs` |
| `src/hook.rs` | `bins/agent-router/src/hook.rs` (rewritten on top of `pipeline`) |
| `src/main.rs` | `bins/agent-router/src/{main,pipeline,mcp}.rs` |

## 6. Historical: v0.3.0 Module-by-Module Code Specifications

The sections below are the v0.3.0 blueprint as written, retained for provenance. The paths
refer to the pre-workspace single-package layout; §5 maps each file to its v2 owner.

### Legacy File 3: `src/types.rs` (Canonical Data Contracts)

Defines every state file and output payload so no module invents ad-hoc JSON keys.

```rust
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentEntry {
    pub id: String,
    pub kind: String, // "agent" or "skill"
    pub path: String,
    pub description: String,
    pub triggers: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RegistryCache {
    pub hash: u64,
    pub entries: BTreeMap<String, AgentEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionState {
    pub session_id: u64,
    #[serde(default)]
    pub host_session_id: Option<String>,
    pub accumulated_tokens: usize,
    pub rolling_summary: String,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 0,
            rolling_summary: String::new(),
        }
    }
}

/// Raw JSON schema expected back from the low-cost LLM
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LlmCompilerOutput {
    pub passthrough: bool,
    pub target_subagent: String,
    pub required_skills: Vec<String>,
    pub compiled_prompt: String,
    pub context_handoff_summary: Option<String>,
}

/// Final deterministic payload emitted to stdout or formatted for lifecycle hooks
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RouterEnvelope {
    pub session_meta: SessionMeta,
    pub routing: RoutingTarget,
    pub payload: PromptPayload,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionMeta {
    pub session_id: u64,
    pub saturation_ratio: f64,
    pub threshold_exceeded: bool,
    pub action: String, // "CONTINUE_SESSION" or "START_NEW_SESSION"
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RoutingTarget {
    pub passthrough: bool,
    pub target_subagent: String,
    pub target_subagent_path: String,
    pub required_skills: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PromptPayload {
    pub context_handoff_summary: Option<String>,
    pub compiled_prompt: String,
}
```

### Legacy File 4: `src/budget.rs` (Step 0: Token Estimator & 35% Circuit Breaker)

Implements zero-dependency BPE token estimation, ledger persistence, turn-by-turn `rolling_summary` breadcrumb accumulation, and the 35% hard ceiling.

```rust
use crate::types::SessionState;
use std::fs;
use std::path::Path;

pub const HARD_CEILING_RATIO: f64 = 0.35;

pub struct BudgetEvaluation {
    pub saturation_ratio: f64,
    pub must_reset_session: bool,
    pub prompt_tokens: usize,
}

/// Zero-dependency BPE token estimator calibrated for code + English (~3.6 chars/token + punctuation weight)
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let char_count = text.chars().count();
    let base_tokens = (char_count as f64 / 3.6).ceil() as usize;
    let punct_or_op = text
        .chars()
        .filter(|c| c.is_ascii_punctuation() || *c == '\n')
        .count();
    base_tokens + (punct_or_op / 3)
}

pub fn load_session(session_file: &Path) -> SessionState {
    fs::read_to_string(session_file)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_session(session_file: &Path, state: &SessionState) -> std::io::Result<()> {
    let serialized = serde_json::to_string_pretty(state)?;
    fs::write(session_file, serialized)
}

pub fn evaluate_budget(
    session: &SessionState,
    raw_prompt: &str,
    base_overhead_tokens: usize,
    max_window_tokens: usize,
) -> BudgetEvaluation {
    let prompt_tokens = estimate_tokens(raw_prompt);
    let projected_total = base_overhead_tokens + session.accumulated_tokens + prompt_tokens;
    let max_safe = max_window_tokens.max(1);
    let saturation_ratio = (projected_total as f64) / (max_safe as f64);

    BudgetEvaluation {
        saturation_ratio,
        must_reset_session: saturation_ratio > HARD_CEILING_RATIO,
        prompt_tokens,
    }
}

pub fn commit_turn_to_ledger(
    session: &mut SessionState,
    eval: &BudgetEvaluation,
    raw_prompt: &str,
    compiled_prompt: &str,
    handoff_summary: Option<String>,
) {
    let compiled_tokens = estimate_tokens(compiled_prompt);
    if eval.must_reset_session {
        session.session_id += 1;
        let handoff = handoff_summary.unwrap_or_default();
        let handoff_tokens = estimate_tokens(&handoff);
        session.accumulated_tokens = handoff_tokens + compiled_tokens;
        session.rolling_summary = handoff;
    } else {
        session.accumulated_tokens += compiled_tokens;
        let snippet: String = raw_prompt
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        if !snippet.is_empty() {
            if !session.rolling_summary.is_empty() {
                session.rolling_summary.push_str(" -> ");
            }
            session.rolling_summary.push_str(&snippet);
            // Cap rolling_summary to last ~600 chars (UTF-8 safe)
            if session.rolling_summary.len() > 600 {
                let mut start = session.rolling_summary.len() - 600;
                while start < session.rolling_summary.len()
                    && !session.rolling_summary.is_char_boundary(start)
                {
                    start += 1;
                }
                session.rolling_summary = session.rolling_summary[start..].to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_reset_strictly_above_35_percent() {
        let state = SessionState {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 35_000,
            rolling_summary: String::new(),
        };
        // 10,000 overhead + 35,000 history + ~100 prompt = 45,100 / 128,000 = 35.23%
        let eval = evaluate_budget(&state, &"a".repeat(360), 10_000, 128_000);
        assert!(eval.must_reset_session);
        assert!(eval.saturation_ratio > 0.35);
    }

    #[test]
    fn stays_in_session_at_or_below_35_percent() {
        let state = SessionState {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 20_000,
            rolling_summary: String::new(),
        };
        // 10,000 overhead + 20,000 history + ~10 prompt = 30,010 / 128,000 = 23.4%
        let eval = evaluate_budget(&state, "fix typo", 10_000, 128_000);
        assert!(!eval.must_reset_session);
    }
}
```

### Legacy File 5: `src/registry.rs` (Step 2: Mtime-Cached Skill & Agent Scanner)

Includes a zero-dependency YAML frontmatter extractor so the router reads `name`, `description`, and `triggers` without pulling in `serde_yaml`, and indexes `.github/agents`, `.github/skills`, `.github/prompts`, and `.github/instructions`.

```rust
use crate::types::{AgentEntry, RegistryCache};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;
use xxhash_rust::xxh3::xxh3_64;

const SEARCH_DIRS: &[(&str, &str)] = &[
    (".agents", "agent"),
    (".claude/agents", "agent"),
    (".github/agents", "agent"),
    ("skills", "skill"),
    (".claude/skills", "skill"),
    (".github/skills", "skill"),
    (".github/prompts", "skill"),
    (".github/instructions", "skill"),
];

pub fn load_or_build_registry(
    repo_root: &Path,
    cache_file: &Path,
) -> BTreeMap<String, AgentEntry> {
    let mut fingerprint = String::new();
    let mut discovered_files: Vec<(PathBuf, &'static str)> = Vec::new();

    for (rel_dir, kind) in SEARCH_DIRS {
        let full_dir = repo_root.join(rel_dir);
        if !full_dir.exists() {
            continue;
        }
        let mut paths: Vec<PathBuf> = WalkDir::new(&full_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        paths.sort();

        for path in paths {
            if let Ok(meta) = fs::metadata(&path) {
                let nanos = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                fingerprint.push_str(&format!("{}:{}|", path.display(), nanos));
                discovered_files.push((path, *kind));
            }
        }
    }

    let current_hash = xxh3_64(fingerprint.as_bytes());

    if let Ok(raw) = fs::read_to_string(cache_file) {
        if let Ok(cached) = serde_json::from_str::<RegistryCache>(&raw) {
            if cached.hash == current_hash {
                return cached.entries;
            }
        }
    }

    let mut entries = BTreeMap::new();
    for (file_path, kind) in discovered_files {
        if let Ok(content) = fs::read_to_string(&file_path) {
            let entry = parse_manifest(repo_root, &file_path, kind, &content);
            entries.insert(entry.id.clone(), entry);
        }
    }

    let cache = RegistryCache {
        hash: current_hash,
        entries: entries.clone(),
    };
    let _ = fs::write(cache_file, serde_json::to_string_pretty(&cache).unwrap_or_default());
    entries
}

/// Zero-dependency parser for YAML frontmatter (`--- ... ---`) with Markdown header fallback
pub fn parse_manifest(
    repo_root: &Path,
    file_path: &Path,
    kind: &str,
    content: &str,
) -> AgentEntry {
    let rel_path = file_path
        .strip_prefix(repo_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/");

    let default_id = if file_path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
        file_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-skill")
            .to_string()
    } else {
        let raw_stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-agent");
        raw_stem
            .strip_suffix(".agent")
            .or_else(|| raw_stem.strip_suffix(".instructions"))
            .or_else(|| raw_stem.strip_suffix(".prompt"))
            .unwrap_or(raw_stem)
            .to_string()
    };

    let mut id = default_id;
    let mut description = String::new();
    let mut triggers = Vec::new();

    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) == Some("---") {
        for line in lines.by_ref() {
            let trimmed = line.trim();
            if trimmed == "---" {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim();
                let val = v.trim().trim_matches('"').trim_matches('\'');
                match key {
                    "name" | "id" if !val.is_empty() => id = val.to_string(),
                    "description" if !val.is_empty() => description = val.to_string(),
                    "triggers" => {
                        let cleaned = val.trim_start_matches('[').trim_end_matches(']');
                        triggers = cleaned
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                    _ => {}
                }
            }
        }
    }

    if description.is_empty() {
        description = content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim().starts_with("---"))
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        if description.len() > 240 {
            let mut end = 240;
            while end > 0 && !description.is_char_boundary(end) {
                end -= 1;
            }
            description.truncate(end);
        }
    }

    AgentEntry {
        id,
        kind: kind.to_string(),
        path: rel_path,
        description,
        triggers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_frontmatter_accurately() {
        let md = "---\nname: sec-auditor\ndescription: Audits Rust unsafe blocks\ntriggers: [security, unsafe, audit]\n---\n# Body";
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/.agents/sec.md"), "agent", md);
        assert_eq!(entry.id, "sec-auditor");
        assert_eq!(entry.description, "Audits Rust unsafe blocks");
        assert_eq!(entry.triggers, vec!["security", "unsafe", "audit"]);
    }

    #[test]
    fn strips_agent_and_instructions_double_extension_when_name_omitted() {
        let md = "---\ndescription: Custom VS Code agent without explicit name\ntriggers: [rust]\n---\n# Body";
        let entry = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/agents/rust-architect.agent.md"),
            "agent",
            md,
        );
        assert_eq!(entry.id, "rust-architect");

        let instr = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/instructions/coding-style.instructions.md"),
            "skill",
            md,
        );
        assert_eq!(instr.id, "coding-style");

        let prompt = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/prompts/refactor.prompt.md"),
            "skill",
            md,
        );
        assert_eq!(prompt.id, "refactor");
    }

    #[test]
    fn truncates_long_multibyte_description_without_panicking() {
        // 2-byte chars make byte index 240 land mid-char; must snap back to a boundary.
        let md = format!("# Title\n{}", "é".repeat(300));
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/no-front.md"), "agent", &md);
        assert!(entry.description.len() <= 240);
        assert!(entry.description.is_char_boundary(entry.description.len()));
    }
}
```

### Legacy File 6: `src/compiler.rs` (Step 1: Prompt Compiler & Fail-Open Fallback)

Implements the complexity passthrough check, the deterministic prompt-engineering system prompt, Hallucination Guard validation (stripping any invented agent/skill IDs), and the offline keyword fallback.

```rust
use crate::types::{AgentEntry, LlmCompilerOutput, SessionState};
use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

/// Fast heuristic check: bypass LLM rewrite if prompt is a short shell/test command
pub fn is_trivial_passthrough(raw_prompt: &str) -> bool {
    let trimmed = raw_prompt.trim();
    let word_count = trimmed.split_whitespace().count();
    let command_prefixes = ["cargo ", "git ", "npm ", "pytest", "ls", "cd ", "/clear", "/help"];
    word_count <= 4 && command_prefixes.iter().any(|p| trimmed.starts_with(p))
}

pub fn compile_and_route(
    raw_prompt: &str,
    registry: &BTreeMap<String, AgentEntry>,
    session: &SessionState,
    must_reset_session: bool,
) -> LlmCompilerOutput {
    if is_trivial_passthrough(raw_prompt) && !must_reset_session {
        return build_fallback_decision(raw_prompt, registry, session, must_reset_session, true);
    }

    match call_llm_endpoint(raw_prompt, registry, session, must_reset_session) {
        Ok(mut decision) => {
            sanitize_hallucinated_ids(&mut decision, registry);
            decision
        }
        Err(err) => {
            eprintln!("[agent-router] LLM compiler unavailable ({}). Failing open with local heuristic.", err);
            build_fallback_decision(raw_prompt, registry, session, must_reset_session, false)
        }
    }
}

fn call_llm_endpoint(
    raw_prompt: &str,
    registry: &BTreeMap<String, AgentEntry>,
    session: &SessionState,
    must_reset_session: bool,
) -> Result<LlmCompilerOutput, String> {
    let api_url = env::var("ROUTER_LLM_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/v1/chat/completions".to_string());
    let api_key = env::var("ROUTER_LLM_KEY").unwrap_or_default();
    let model = env::var("ROUTER_LLM_MODEL").unwrap_or_else(|_| "qwen2.5-coder:7b".to_string());
    let timeout_ms: u64 = env::var("ROUTER_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);

    let compact_registry: Vec<_> = registry
        .values()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "kind": e.kind,
                "description": e.description,
                "triggers": e.triggers
            })
        })
        .collect();

    let system_prompt = format!(
        "You are a deterministic Prompt Compiler and Subagent Router.
Respond ONLY with a valid JSON object matching this exact schema:
{{
  \"passthrough\": boolean,
  \"target_subagent\": \"string (exact id of an agent from Available Registry, or 'default')\",
  \"required_skills\": [\"string (exact ids of skills from Available Registry)\"],
  \"compiled_prompt\": \"string\",
  \"context_handoff_summary\": \"string or null\"
}}

Available Registry:
{}

Prior Session Summary:
\"{}\"

Session Reset Triggered (>35% context usage): {}

Prompt Engineering Rules for `compiled_prompt`:
1. Transform the user's raw request into a structured specification using XML tags:
   <objective>Clear, concrete engineering goal.</objective>
   <constraints>Guardrails: do not invent dependencies, preserve existing APIs, minimize diff surface.</constraints>
   <verification>Exact test or build commands required to prove correctness.</verification>
2. Select `target_subagent` ONLY from entries with kind=='agent'. If none fit, output \"default\".
3. Select `required_skills` ONLY from entries with kind=='skill'.
4. If Session Reset Triggered is true, compress Prior Session Summary + current intent into `context_handoff_summary` (max 150 words).",
        serde_json::to_string(&compact_registry).unwrap_or_default(),
        session.rolling_summary,
        must_reset_session
    );

    let body = serde_json::json!({
        "model": model,
        "temperature": 0.0,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": raw_prompt }
        ]
    });

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(timeout_ms))
        .timeout_read(Duration::from_millis(timeout_ms))
        .build();

    let mut req = agent.post(&api_url).set("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", api_key));
    }

    let resp: serde_json::Value = req
        .send_json(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;

    let raw_content = resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "Missing choices[0].message.content".to_string())?;

    let cleaned = strip_markdown_fences(raw_content);
    serde_json::from_str::<LlmCompilerOutput>(cleaned).map_err(|e| e.to_string())
}

fn strip_markdown_fences(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_end_matches("```").trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_end_matches("```").trim()
    } else {
        trimmed
    }
}

/// Prevents hallucinated agent or skill names from ever leaving the compiler
pub fn sanitize_hallucinated_ids(
    decision: &mut LlmCompilerOutput,
    registry: &BTreeMap<String, AgentEntry>,
) {
    let valid_agent = registry
        .get(&decision.target_subagent)
        .map(|e| e.kind == "agent")
        .unwrap_or(false);

    if !valid_agent {
        decision.target_subagent = "default".to_string();
    }

    decision.required_skills.retain(|skill_id| {
        registry
            .get(skill_id)
            .map(|e| e.kind == "skill")
            .unwrap_or(false)
    });
}

/// Offline / Fail-Open deterministic compiler & keyword matcher
pub fn build_fallback_decision(
    raw_prompt: &str,
    registry: &BTreeMap<String, AgentEntry>,
    session: &SessionState,
    must_reset_session: bool,
    passthrough: bool,
) -> LlmCompilerOutput {
    let lower_prompt = raw_prompt.to_lowercase();
    let mut matched_agent = "default".to_string();
    let mut matched_skills = Vec::new();

    for entry in registry.values() {
        let hit = entry.triggers.iter().any(|t| lower_prompt.contains(&t.to_lowercase()))
            || lower_prompt.contains(&entry.id.to_lowercase());
        if hit {
            if entry.kind == "agent" && matched_agent == "default" {
                matched_agent = entry.id.clone();
            } else if entry.kind == "skill" {
                matched_skills.push(entry.id.clone());
            }
        }
    }

    let compiled_prompt = if passthrough {
        raw_prompt.to_string()
    } else {
        format!(
            "<objective>\n{}\n</objective>\n<constraints>\n- Follow existing repository patterns and conventions.\n- Do not introduce unrequested dependencies or unrelated refactors.\n</constraints>\n<verification>\n- Run relevant test suites and verify zero compiler/linter errors before completing.\n</verification>",
            raw_prompt.trim()
        )
    };

    let context_handoff_summary = if must_reset_session {
        Some(format!(
            "Prior session reset at >35% context ceiling. Previous summary: {}",
            if session.rolling_summary.is_empty() {
                "None recorded."
            } else {
                &session.rolling_summary
            }
        ))
    } else {
        None
    };

    LlmCompilerOutput {
        passthrough,
        target_subagent: matched_agent,
        required_skills: matched_skills,
        compiled_prompt,
        context_handoff_summary,
    }
}
```

### Legacy File 7: `src/dispatcher.rs` (Step 3: Harness-Agnostic Envelope & CLI Spawner)

```rust
use crate::budget::BudgetEvaluation;
use crate::types::{
    AgentEntry, LlmCompilerOutput, PromptPayload, RouterEnvelope, RoutingTarget, SessionMeta,
    SessionState,
};
use std::collections::BTreeMap;
use std::env;
use std::process::{Command, Stdio};

pub fn build_envelope(
    session: &SessionState,
    eval: &BudgetEvaluation,
    decision: LlmCompilerOutput,
    registry: &BTreeMap<String, AgentEntry>,
) -> RouterEnvelope {
    let target_subagent_path = registry
        .get(&decision.target_subagent)
        .map(|e| e.path.clone())
        .unwrap_or_else(|| "default".to_string());

    let skill_paths: Vec<String> = decision
        .required_skills
        .iter()
        .filter_map(|id| registry.get(id).map(|e| e.path.clone()))
        .collect();

    RouterEnvelope {
        session_meta: SessionMeta {
            session_id: session.session_id,
            saturation_ratio: (eval.saturation_ratio * 1000.0).round() / 1000.0,
            threshold_exceeded: eval.must_reset_session,
            action: if eval.must_reset_session {
                "START_NEW_SESSION".to_string()
            } else {
                "CONTINUE_SESSION".to_string()
            },
        },
        routing: RoutingTarget {
            passthrough: decision.passthrough,
            target_subagent: decision.target_subagent,
            target_subagent_path,
            required_skills: skill_paths,
        },
        payload: PromptPayload {
            context_handoff_summary: decision.context_handoff_summary,
            compiled_prompt: decision.compiled_prompt,
        },
    }
}

pub fn execute_dispatch(envelope: &RouterEnvelope) -> Result<i32, String> {
    if let Ok(harness_bin) = env::var("ROUTER_HARNESS_BIN") {
        let final_prompt = match &envelope.payload.context_handoff_summary {
            Some(handoff) if envelope.session_meta.threshold_exceeded => format!(
                "<session_handoff>\n{}\n</session_handoff>\n\n{}",
                handoff, envelope.payload.compiled_prompt
            ),
            _ => envelope.payload.compiled_prompt.clone(),
        };

        let status = Command::new(harness_bin)
            .env("ROUTED_SESSION_ID", envelope.session_meta.session_id.to_string())
            .env("ROUTED_SESSION_ACTION", &envelope.session_meta.action)
            .env("ROUTED_SUBAGENT", &envelope.routing.target_subagent)
            .env("ROUTED_SUBAGENT_PATH", &envelope.routing.target_subagent_path)
            .env("ROUTED_SKILLS", envelope.routing.required_skills.join(","))
            .arg(&final_prompt)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to spawn harness binary: {}", e))?;

        Ok(status.code().unwrap_or(0))
    } else {
        let json = serde_json::to_string_pretty(envelope).map_err(|e| e.to_string())?;
        println!("{}", json);
        Ok(0)
    }
}
```

### Legacy File 8: `src/hook.rs` (Step 3 Mode C: Zero-Dependency Lifecycle Hook Adapter)

Implements synchronous stdin/stdout lifecycle hook handling (`agent-router --hook`) for VS Code GitHub Copilot, Claude Code, and Antigravity (AGY).

```rust
use crate::budget::estimate_tokens;
use crate::run_pipeline_with_context;
use crate::types::RouterEnvelope;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Format the [`RouterEnvelope`] into an imperative context block optimized for VS Code Copilot (`runSubagent`) and Claude Code.
pub fn format_hook_context(envelope: &RouterEnvelope) -> String {
    let mut sections = Vec::new();

    sections.push(format!(
        "<router_meta session_id=\"{}\" saturation=\"{:.3}\" action=\"{}\" subagent=\"{}\" subagent_path=\"{}\" skills=\"{}\" passthrough=\"{}\" />",
        envelope.session_meta.session_id,
        envelope.session_meta.saturation_ratio,
        envelope.session_meta.action,
        envelope.routing.target_subagent,
        envelope.routing.target_subagent_path,
        envelope.routing.required_skills.join(","),
        envelope.routing.passthrough,
    ));

    // Imperative routing directive tailored for VS Code Copilot `runSubagent` + Claude Code `Task`
    if envelope.routing.target_subagent != "default" || !envelope.routing.required_skills.is_empty() {
        let mut directives = Vec::new();
        if envelope.routing.target_subagent != "default" {
            directives.push(format!(
                "- MANDATORY SUBAGENT DELEGATION: You MUST delegate this task to the custom agent `{name}` (defined at `{path}`). In VS Code Copilot, invoke the `runSubagent` (`agent/runSubagent`) tool with `agentName: \"{name}\"` and pass the `<objective>`, `<constraints>`, and `<verification>` blocks below as its `prompt`. If `runSubagent` is unavailable, read `{path}` first and strictly adopt its persona and constraints.",
                name = envelope.routing.target_subagent,
                path = envelope.routing.target_subagent_path
            ));
        }
        if !envelope.routing.required_skills.is_empty() {
            directives.push(format!(
                "- MANDATORY SKILLS: Before writing code, read and follow the skill instructions at: `{}`.",
                envelope.routing.required_skills.join("`, `")
            ));
        }
        sections.push(format!(
            "<routing_directive>\n{}\n</routing_directive>",
            directives.join("\n")
        ));
    }

    if let Some(handoff) = &envelope.payload.context_handoff_summary {
        if !handoff.is_empty() {
            sections.push(format!("<session_handoff>\n{}\n</session_handoff>", handoff));
        }
    }

    sections.push(envelope.payload.compiled_prompt.clone());
    sections.join("\n\n")
}

/// Extract the latest user prompt from an Antigravity JSONL transcript if present.
fn extract_prompt_from_transcript(transcript_path: &str) -> Option<String> {
    let content = fs::read_to_string(transcript_path).ok()?;
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let is_user_input = entry
            .get("type")
            .and_then(|t| t.as_str())
            .map(|t| t == "USER_INPUT")
            .unwrap_or(false);
        if is_user_input {
            if let Some(text) = entry.get("content").and_then(|c| c.as_str()) {
                if !text.trim().is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// Estimate real session token usage directly from the harness transcript file (`transcript_path` in VS Code / Claude).
fn estimate_transcript_tokens(req: &Value) -> Option<usize> {
    let path_str = req
        .get("transcript_path")
        .or_else(|| req.get("transcriptPath"))
        .and_then(|p| p.as_str())?;

    let content = fs::read_to_string(path_str).ok()?;
    Some(estimate_tokens(&content))
}

/// Extract the host harness's native session ID (`sessionId` in VS Code, `session_id` in Claude/Copilot CLI).
fn extract_host_session_id(req: &Value) -> Option<String> {
    req.get("sessionId")
        .or_else(|| req.get("session_id"))
        .or_else(|| req.get("conversationId"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

fn is_agy_hook(input: &Value) -> bool {
    input.get("invocationNum").is_some()
        || input.get("workspacePaths").is_some()
        || input.get("transcriptPath").is_some()
        || input.get("conversationId").is_some()
}

pub fn run_stdio_hook(default_repo_root: &Path) -> io::Result<()> {
    let mut raw_input = String::new();
    io::stdin().read_to_string(&mut raw_input)?;

    let mut stdout = io::stdout();
    let trimmed = raw_input.trim();
    if trimmed.is_empty() {
        writeln!(stdout, "{{}}")?;
        stdout.flush()?;
        return Ok(());
    }

    let req: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            writeln!(stdout, "{{}}")?;
            stdout.flush()?;
            return Ok(());
        }
    };

    let repo_root: PathBuf = req
        .get("cwd")
        .and_then(|c| c.as_str())
        .or_else(|| req.pointer("/workspacePaths/0").and_then(|p| p.as_str()))
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(|| default_repo_root.to_path_buf());

    let raw_prompt = req
        .get("prompt")
        .and_then(|p| p.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            req.get("transcriptPath")
                .and_then(|t| t.as_str())
                .and_then(extract_prompt_from_transcript)
        })
        .unwrap_or_default();

    if raw_prompt.trim().is_empty() {
        writeln!(stdout, "{{}}")?;
        stdout.flush()?;
        return Ok(());
    }

    let observed_tokens = estimate_transcript_tokens(&req);
    let host_session_id = extract_host_session_id(&req);

    let response = match run_pipeline_with_context(
        &repo_root,
        &raw_prompt,
        observed_tokens,
        host_session_id,
    ) {
        Ok(envelope) => {
            let context = format_hook_context(&envelope);
            if is_agy_hook(&req) {
                let agy_msg = if envelope.session_meta.threshold_exceeded {
                    format!(
                        "<circuit_breaker_alert>\nSTOP: Context saturation reached {:.1}% (>35% hard ceiling). State handoff saved to `.router/HANDOFF.md`. Start a new conversation.\n</circuit_breaker_alert>\n\n{}",
                        envelope.session_meta.saturation_ratio * 100.0,
                        context
                    )
                } else {
                    context
                };
                json!({
                    "injectSteps": [
                        {
                            "ephemeralMessage": agy_msg
                        }
                    ]
                })
            } else if envelope.session_meta.threshold_exceeded {
                let event_name = req
                    .get("hookEventName")
                    .or_else(|| req.get("hook_event_name"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("UserPromptSubmit");
                let stop_msg = format!(
                    "⚡ AgentRouter Circuit Breaker: Context reached {:.1}% (>35% ceiling). State handoff & compiled prompt saved to .router/HANDOFF.md. Click '+' (New Chat) or run `/clear` and re-submit your prompt.",
                    envelope.session_meta.saturation_ratio * 100.0
                );
                // Compatible with BOTH VS Code Copilot (`continue: false`, `stopReason`) and Claude Code (`decision: "block"`, `reason`)
                json!({
                    "continue": false,
                    "stopReason": stop_msg,
                    "systemMessage": stop_msg,
                    "decision": "block",
                    "reason": stop_msg,
                    "hookSpecificOutput": {
                        "hookEventName": event_name,
                        "additionalContext": context
                    }
                })
            } else {
                let event_name = req
                    .get("hookEventName")
                    .or_else(|| req.get("hook_event_name"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("UserPromptSubmit");
                let skills_label = if envelope.routing.required_skills.is_empty() {
                    "none".to_string()
                } else {
                    envelope.routing.required_skills.join(",")
                };
                let status_banner = format!(
                    "⚡ AgentRouter [{:.1}%/35% ctx | subagent: {} | skills: {}]",
                    envelope.session_meta.saturation_ratio * 100.0,
                    envelope.routing.target_subagent,
                    skills_label
                );
                json!({
                    "continue": true,
                    "systemMessage": status_banner,
                    "hookSpecificOutput": {
                        "hookEventName": event_name,
                        "additionalContext": context
                    }
                })
            }
        }
        Err(e) => {
            eprintln!("[agent-router] Hook pipeline error (failing open): {}", e);
            json!({})
        }
    };

    writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
    stdout.flush()?;
    Ok(())
}
```

### Legacy File 9: `src/main.rs` (CLI Entrypoint & Pipeline Orchestrator)

```rust
mod budget;
mod compiler;
mod dispatcher;
mod hook;
mod registry;
mod types;

use std::env;
use std::fs;
use std::path::Path;
use types::RouterEnvelope;

pub fn run_pipeline(repo_root: &Path, raw_prompt: &str) -> Result<RouterEnvelope, String> {
    run_pipeline_with_context(repo_root, raw_prompt, None, None)
}

pub fn run_pipeline_with_context(
    repo_root: &Path,
    raw_prompt: &str,
    observed_transcript_tokens: Option<usize>,
    host_session_id: Option<String>,
) -> Result<RouterEnvelope, String> {
    let router_dir = repo_root.join(".router");
    fs::create_dir_all(&router_dir).map_err(|e| e.to_string())?;

    let base_overhead: usize = env::var("ROUTER_BASE_OVERHEAD_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let max_window: usize = env::var("ROUTER_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128_000);

    let session_file = router_dir.join("session.json");
    let handoff_file = router_dir.join("HANDOFF.md");
    let mut session = budget::load_session(&session_file);

    // If the host harness started a brand new session (e.g. after `/clear` or clicking `+`),
    // reset token counter while keeping any pending handoff intact for the first turn.
    if let Some(ref new_host_id) = host_session_id {
        if session.host_session_id.as_ref() != Some(new_host_id) {
            if session.host_session_id.is_some() || handoff_file.exists() {
                session.accumulated_tokens = 0;
            }
            session.host_session_id = Some(new_host_id.clone());
        }
    }

    // If the hook provided a live transcript path, sync accumulated_tokens with real transcript size
    if let Some(real_tokens) = observed_transcript_tokens {
        if real_tokens > session.accumulated_tokens {
            session.accumulated_tokens = real_tokens;
        }
    }

    // Step 0: Evaluate Context Budget (35% Ceiling)
    let eval = budget::evaluate_budget(&session, raw_prompt, base_overhead, max_window);

    // Step 2: Load or Build XXH3 Mtime-Cached Capability Registry
    let cache_file = router_dir.join("registry.json");
    let registry = registry::load_or_build_registry(repo_root, &cache_file);

    // Step 1: Compile Prompt & Select Subagent/Skills (Fail-Open Resilient)
    let mut decision =
        compiler::compile_and_route(raw_prompt, &registry, &session, eval.must_reset_session);

    // Persist or consume `.router/HANDOFF.md` across blocked -> fresh session transitions
    if eval.must_reset_session {
        if !handoff_file.exists() {
            if let Some(ref handoff) = decision.context_handoff_summary {
                let _ = fs::write(&handoff_file, handoff);
            }
        }
    } else if handoff_file.exists() && !decision.passthrough {
        if let Ok(pending_handoff) = fs::read_to_string(&handoff_file) {
            let trimmed = pending_handoff.trim();
            if !trimmed.is_empty() && decision.context_handoff_summary.is_none() {
                decision.context_handoff_summary = Some(trimmed.to_string());
            }
        }
        let _ = fs::remove_file(&handoff_file);
    }

    // Commit Updated Token Ledger
    let prior_tokens = session.accumulated_tokens;
    budget::commit_turn_to_ledger(
        &mut session,
        &eval,
        raw_prompt,
        &decision.compiled_prompt,
        decision.context_handoff_summary.clone(),
    );
    // If we are in Hook mode (`host_session_id.is_some()`) and blocked the turn (`must_reset_session`),
    // keep `accumulated_tokens` pinned until `host_session_id` actually changes on New Chat (`+`)!
    if eval.must_reset_session && host_session_id.is_some() {
        session.accumulated_tokens = prior_tokens;
    }
    budget::save_session(&session_file, &session).map_err(|e| e.to_string())?;

    eprintln!(
        "[agent-router] prompt_tokens={} projected_saturation={:.3} reset={}",
        eval.prompt_tokens, eval.saturation_ratio, eval.must_reset_session
    );

    // Step 3: Build Harness-Agnostic Envelope
    Ok(dispatcher::build_envelope(&session, &eval, decision, &registry))
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let repo_root = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());

    if args.is_empty() {
        eprintln!("Usage:\n  agent-router \"<raw_prompt>\"\n  agent-router --hook\n  agent-router --sync-tokens <exact_token_count>");
        std::process::exit(1);
    }

    match args[0].as_str() {
        "--hook" => {
            if let Err(e) = hook::run_stdio_hook(&repo_root) {
                eprintln!("[agent-router] Hook error: {}", e);
                std::process::exit(1);
            }
        }
        "--sync-tokens" => {
            match args.get(1).and_then(|s| s.parse::<usize>().ok()) {
                Some(val) => {
                    // The ledger may not exist yet (first calibration before any routed
                    // turn), so ensure the state directory is present before writing.
                    let router_dir = repo_root.join(".router");
                    if let Err(e) = fs::create_dir_all(&router_dir) {
                        eprintln!("[agent-router] cannot create {}: {}", router_dir.display(), e);
                        std::process::exit(1);
                    }
                    let session_file = router_dir.join("session.json");
                    let mut session = budget::load_session(&session_file);
                    session.accumulated_tokens = val;
                    if let Err(e) = budget::save_session(&session_file, &session) {
                        eprintln!("[agent-router] failed to persist token ledger: {}", e);
                        std::process::exit(1);
                    }
                    eprintln!("[agent-router] ledger calibrated to {} tokens", val);
                }
                None => {
                    eprintln!("Usage: agent-router --sync-tokens <exact_token_count>");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            let raw_prompt = args.join(" ");
            match run_pipeline(&repo_root, &raw_prompt) {
                Ok(envelope) => match dispatcher::execute_dispatch(&envelope) {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("[agent-router] Dispatch error: {}", e);
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("[agent-router] Pipeline error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}
```

## 7. Historical: v0.3.0 Step-by-Step Build & Verification Sequence

The v2 command sequence is §5 above; the transcript below is kept as the original single-package
runbook (its `cargo test` / `cargo run` invocations are shorthand for `cargo test --workspace` and
`cargo run -p agent-router`).

### 1. Scaffold and Verify Unit Tests

> Zero network calls required

Create the files above and run the built-in test suite to verify the 35% context ceiling math and frontmatter parser:

```bash
cargo test
```

### 2. Verify Fail-Open Offline Execution

> Tests local fallback when no LLM is running

Create a sample agent in `.agents/rust-architect.md`, then run the binary with an unreachable port to verify fail-open behavior:

```bash
mkdir -p .agents skills/tdd
printf -- "---\nname: rust-architect\ndescription: Systems architect\ntriggers: [rust, borrow, lifetime]\n---\n" > .agents/rust-architect.md
printf -- "---\nname: tdd-workflow\ndescription: Red-Green-Refactor\ntriggers: [test, tdd]\n---\n" > skills/tdd/SKILL.md
ROUTER_LLM_URL="http://127.0.0.1:59999/v1/chat/completions" ROUTER_TIMEOUT_MS=200 cargo run -- "fix rust lifetime in parser and add test"
```

Verify that stdout outputs valid JSON selecting `rust-architect` and `skills/tdd/SKILL.md` in under 250ms.

### 3. Verify 35% Hard Reset Trigger

> Sync the ledger near the threshold (e.g., 36,000 tokens + 10,000 base overhead = 46,000 / 128,000 = 35.9%) and run a prompt:

```bash
cargo run -- --sync-tokens 36000
cargo run -- "implement next module"
```

Confirm `session_meta.threshold_exceeded` is `true`, `session_meta.action` is `"START_NEW_SESSION"`, and `session_meta.session_id` increments to `2`.

### 4. Verify Lifecycle Hook Execution (`--hook`)

> Pipe a `UserPromptSubmit` hook payload directly to `--hook` mode:

```bash
printf '{"hook_event_name":"UserPromptSubmit","prompt":"fix rust lifetime in parser and add test"}\n' | cargo run -- --hook
```

Confirm a single-line JSON response containing `hookSpecificOutput.additionalContext` with `<router_meta ... />` and `<objective>`.
