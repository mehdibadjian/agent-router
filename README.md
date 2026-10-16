# agent-router

Zero-runtime-dependency prompt pre-processor and subagent dispatcher for AI coding harnesses.

A single static Rust binary that sits in front of an expensive "System 2" coding model and does the cheap work first: it enforces a hard context-saturation ceiling, compiles terse prompts into structured engineering specs, indexes the repository's agents and skills with a sub-millisecond cache, and routes the result to the best-matched subagent — without Python, Node, or Docker on the host.

**Status:** Phases 1–4 implemented (pipeline, fail-open compiler, harness adapters, CI matrix). Static-linking claims are enforced by CI but not yet observed passing end-to-end; see [Verification status](#verification-status).

Docs: [Business Requirements](docs/BRD.md) · [Implementation Plan](docs/IMPLEMENTATION_PLAN.md)

## How it works

```
raw prompt → Step 0 budget check → Step 2 registry cache → Step 1 compile+route → Step 3 dispatch
```

| Step | Module | Role |
|---|---|---|
| 0 | `src/budget.rs` | Estimates tokens with no BPE library; sets `must_reset_session` when projected usage exceeds 35% of the window; maintains the session ledger and handoff summary. |
| 2 | `src/registry.rs` | Walks agent/skill directories, parses YAML frontmatter with a hand-rolled reader, and caches `.router/registry.json` behind an XXH3 fingerprint of paths + nanosecond mtimes. |
| 1 | `src/compiler.rs` | Calls any OpenAI-compatible endpoint to rewrite the prompt into `<objective>` / `<constraints>` / `<verification>`. Bypasses the call for trivial commands. Rejects invented agent IDs. Falls open on any failure. |
| 3 | `src/dispatcher.rs` | Emits the envelope via CLI subprocess, clean stdout JSON, or both. |
| 3C | `src/mcp.rs` | Synchronous JSON-RPC 2.0 MCP stdio server exposing one tool, `route_and_compile`. |

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
agent-router --mcp                                # Mode C: JSON-RPC 2.0 server on stdio
agent-router --sync-tokens <exact_count>          # calibrate the ledger from real API usage
```

Route a prompt and inspect the contract it produces:

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

Wrap an existing harness (Mode A). The compiled prompt is passed as an argument, the envelope is not printed, and the harness exit code is propagated:

```bash
ROUTER_HARNESS_BIN=claude agent-router "add pagination to /users"
```

## Configuration

All configuration is via environment variables. API keys are read from the environment only and are never written to `.router/` state or passed as process arguments.

| Variable | Default | Purpose |
|---|---|---|
| `ROUTER_LLM_URL` | `http://127.0.0.1:11434/v1/chat/completions` | Any OpenAI-compatible endpoint (Ollama, llama.cpp, vLLM, MLX, Groq, DeepSeek, Gemini). |
| `ROUTER_LLM_MODEL` | `qwen2.5-coder:7b` | Model name sent to the endpoint. |
| `ROUTER_LLM_KEY` | *(empty)* | Bearer token; omitted from the request when unset. |
| `ROUTER_TIMEOUT_MS` | `3000` | Connect **and** read timeout. Exceeding it triggers fail-open. |
| `ROUTER_BASE_OVERHEAD_TOKENS` | `10000` | Hidden tokens the harness injects (system prompt, MCP defs, rules). |
| `ROUTER_MAX_TOKENS` | `128000` | Denominator for the saturation ratio. |
| `ROUTER_HARNESS_BIN` | *(unset)* | When set, enables Mode A subprocess dispatch. |

Injected into the spawned harness under Mode A: `ROUTED_SESSION_ID`, `ROUTED_SESSION_ACTION`, `ROUTED_SUBAGENT`, `ROUTED_SUBAGENT_PATH`, `ROUTED_SKILLS`.

## Discovery conventions

Scanned for `.md` manifests, with a hand-written frontmatter parser (no `serde_yaml`) reading only `name`/`id`, `description`, and `triggers`:

| Directory | Kind |
|---|---|
| `.agents/` | agent |
| `.claude/agents/` | agent |
| `skills/` | skill |
| `.claude/skills/` | skill |

`SKILL.md` files inherit their ID from the containing directory when no `name` is declared.

## Dependencies

Exactly five crates, pinned in `Cargo.lock`; `ureq` is configured `default-features = false, features = ["tls", "json"]` so TLS is pure-Rust (`rustls` + `ring`) rather than OpenSSL. No async runtime, no tokenizer C library, no clap.

```
serde 1.0.229 · serde_json 1.0.154 · ureq 2.12.1 · xxhash-rust 0.8.19 · walkdir 2.5.0
```

State lives in `.router/session.json` and `.router/registry.json`; both are gitignored and disposable.

## Design constraints worth knowing

- **Stdout is a protocol surface.** Diagnostics go to stderr exclusively, so Mode B and Mode C output stays machine-parseable.
- **Fail-open is unconditional.** An unreachable endpoint, a timeout, or malformed LLM JSON logs a warning and routes heuristically — the pipeline never blocks the developer.
- **Hallucinated IDs are stripped.** Anything the model returns that is not a registered agent/skill of the correct `kind` is discarded, falling back to `default`.
- **`panic = "abort"` is deliberately narrow.** The release profile aborts on panic, so a `unwrap()` on externally-controlled data would hard-crash rather than fail open. Byte-offset slicing on untrusted UTF-8 is a real hazard here — `registry.rs` truncates at the nearest `is_char_boundary`, and `tests/pipeline.rs` guards the fail-open paths against exit code 134.

## Verification status

Measured on this machine (Linux x86_64, glibc host, Rust 1.98.1):

- `cargo test --all-targets`: **18 passing** (4 unit, 14 integration) — budget ceiling math, frontmatter parsing, multibyte truncation, MCP handshake/notification/garbage handling, fail-open, fence stripping, hallucination sanitising, passthrough gate, Mode A exit-code propagation, cache invalidation.
- Cold and warm invocation: **~1 ms**, cache hit validated by stable XXH3 fingerprint.
- Release binary: **1.73 MiB** (NFR < 5 MiB). Peak RSS: **~8 MiB** (NFR < 15 MB).

Not yet observed on this machine:

- **Static linkage is unverified.** The musl target could not be installed here (rustup mirror returned 404), so `ldd` was only checked against the glibc host build, which is expected to be dynamic. The `Assert fully static linkage` gate in CI is the first real test of this claim.
- The `rustfmt` and `clippy` steps run informationally (`continue-on-error`) because the sources were transcribed from the implementation plan rather than emitted by `rustfmt`.

## CI

`.github/workflows/ci.yml` gates `cargo test --all-targets`, then builds and asserts the NFRs per platform: Linux musl x86_64/aarch64 (`ldd` + `readelf` forbidden-library scan + 5 MiB ceiling), Windows msvc-static, and a macOS universal binary created with `lipo`. Artifacts are bundled into `agent-router-0.1.0-static-binaries.tar.gz`.

## License

Not yet specified.
