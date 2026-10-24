# Rhema

<div align="center">
<b>ῥῆμα, "the spoken word": words that do things.</b>
</div>

<br>

<div align="center">
<a href="https://rhema.rs">
<img src=".github/img/a1-breath-dark.png" width="100">

<br>

rhema.rs
</a>
</div>

<br>

Rhema is a small, embeddable agent framework in Rust.

It consists of multiple components, going from `rhema-code`, a terminal-based coding agent frontend, down to `rhema-core`, a simple library allowing step-based interactions with LLMs.

## Components

<img align="center" src=".github/img/graph.png">

### Rhema Code (Terminal UI)

<img align="center" src=".github/img/rhema-code.png">

Get Rhema Code: 
```
curl -fsSL https://get.rhema.rs/code | sh
```

See <a href="./crates/rhema-code/README.md">Rhema Code README.md</a> for more.

### Rhema Claw

Still Work-In-Progress.
See [Rhema Claw README.md](./crates/rhema-claw/README.md) for more.

### The client tools (`tools/`)

A separate Cargo workspace holding what runs on the *user's machine*
and talks to a claw-shaped daemon: the shared contract crate
(`rhema-tools-contract` — the scoped agent-token allowlist, the
version header, and the conformance suite) and the tools themselves
(`notify` today). The arrow runs tool → daemon only, over the scoped
agent token, never the reverse; `specs/rhema-tools.md` is the design.

### All crates

| Crate | What it does |
|---|---|
| `rhema-core` | The step engine. `Agent::step` runs one exchange with a model over a caller-owned, serializable `Session`, and never loops. Around it: the `ModelProvider`, `ToolResolver`, and `EventSink` seams, `run_turn`, a scripted `MockProvider` behind the `mock` feature, and commit-before-execute (`CommitHook`, per-tool `ReplaySafety`) so a crash inside a tool call leaves a resumable intent |
| `rhema-provider` | Shared base for the adapters: the `ModelCard` catalog row, `Pricing`, the SSE decoder, the `InteractiveLogin` flow seam, and the `Retrying` decorator that re-issues transient model failures with backoff |
| `rhema-provider-google` | Gemini adapter: streaming SSE, native tool calling, live model listing |
| `rhema-provider-anthropic` | Anthropic Messages adapter: streaming SSE, native tool use, live model listing; also serves Anthropic-compatible routes (Z.ai's GLM Coding Plan) by base override and bearer auth, and reports the plan's 5-hour/weekly quota on the Z.ai hosts |
| `rhema-provider-openai-compatible` | Any OpenAI-compatible chat-completions server (OpenAI, vLLM, LM Studio): streaming, live listing |
| `rhema-provider-openrouter` | OpenRouter adapter: the text-chat catalog with per-model pricing and context lengths |
| `rhema-provider-llama-cpp` | llama.cpp `llama-server` adapter: streaming chat completions, optional bearer key |
| `rhema-provider-copilot` | GitHub Copilot adapter: the OpenAI-shaped proxy, driven by a device-flow OAuth token |
| `rhema-config` | The XDG config file: named profiles of provider instances, stored keys, the active pair, the last-used model |
| `rhema-rsd` | Resource-scoped dispatch: at most one live handler per resource key, a live inbox delivered mid-run, revision-guarded checkpoints, leases with TTL and crash recovery |
| `rhema-memory` | Cross-session memory: one record per fact behind a `MemoryStore` contract, surfaced as `memory_*` tools the model calls itself; records carry a validity window, decay, provenance, a subject key, and a verify hint, and are superseded or retracted — never silently stale, never silently deleted |
| `rhema-identity` | Accounts: one record per external identity, each holding one write-only secret behind a use-never-read contract |
| `rhema-autonomous-agent` | The resident-agent core tier: model-free `Source` detection, the lease-driven `Driver` loop, allowlist-cut tool resolvers over `TOOL.yaml` manifests, and `SessionContext::commit_hook`, which binds a run's lease to the engine's commit hook |
| `rhema-agent-tools` | The agent toolkit on rhema-core's tool abstraction: one approval-gated `bash` tool, the `Approval` seam, markdown skills, paste expansion, revive-on-resume for calls a crash left unanswered, and the Jinja system-prompt renderer |
| `rhema-coding-agent` | The coding-agent driver: the turn loop over the agent toolkit, session save/resume, and a system prompt assembled from the editable SYSTEM.md template with tools and skills rendered in |
| `rhema-subagents` | The subagent extension: markdown subagent definitions plus the one `subagent` tool that runs them — a fresh agent on a fresh session whose report comes back as the tool result, with the parent turn's approval hook inherited and nesting bounded — plus a built-in `general-purpose` definition, so the tool is always there; nothing in the tree depends on it |
| `rhema-transcript` | The bounded view over an unbounded session: the request decorator that rewrites a step's messages under a byte budget — the current turn and the recent past whole, the aged prefix the stepped compaction block (append-only sections, moved only at hysteresis-gated flushes so the request prefix stays cache-stable between them). Sections carry an extractive digest by default; a summary worker (`CodingAgentConfig::summary_worker`) rewrites them densely at flush time, never blocking or failing the request. The `zoom` tool reads any original message back; the mirror reports the benchmark stats (request counts, flushes, block size, the providers' own prompt-cache hit rate) and a per-category usage breakdown — system prompt, skills, tool calls in/out, user messages, model output — that rhema-code shows on the Usage page. On by default in the coding agent, `rhema acp`, and the claw; `--no-transcript` (or the claw's `transcript: false`) sends the whole history |
| `rhema-acp` | The coding agent as an Agent Client Protocol subprocess, so ACP editors (T3 Code, Zed, JetBrains, Neovim, Emacs, VS Code) drive it without a per-editor plugin |
| `rhema-cloud-agent` | The harness the cloud frontends share: the filesystem-less assembly, the client approval gate, the sandbox seam, the per-session config, and the client wire protocol — no platform dependency enters this crate, and the offline end-to-end drives a full turn (approval round-trip, denial, crash mid-command and its resume) through it |
| `rhema-vercel-agent` | The coding agent on Vercel: an axum server a Vercel container runs natively, speaking the Cloudflare DO's client protocol byte for byte, with `bash` in a persistent named Vercel Sandbox (REST, polled) and sessions checkpointed into Postgres through the same revision-guarded Store contract |
| `rhema-cloudflare-code` | The coding agent as a Cloudflare Durable Object: WebSocket chat with streaming events, `bash` in a Cloudflare Sandbox container, revision-guarded session checkpoints in the DO's SQLite storage that survive DO and container eviction, approvals as a per-session policy plus a WebSocket round-trip with deny-on-timeout. One provider (Anthropic) over host fetch; the workspace persists across idle sleeps, a full container eviction resumes through the engine's revive machinery (`deploy/cloudflare/` has the walk-through) |
| `rhema-code` | The `rhema` binary: the full-screen terminal UI (the footer's context gauge tracks the streaming turn live, from the providers' own running usage tallies), `rhema run` for the pipeable one-shot (exit 0 success, 2 when the model backend failed — a rate limit names its `retry-after` and does not burn retries; `--save` checkpoints the session per tool round, and a failed turn always leaves the partial session for `--resume`), `rhema acp` for editors, `rhema system edit/show/restore` for the prompt the agent works from |
| `rhema-tools-contract` | The client tools' contract (`tools/` workspace): the scoped agent-token allowlist, the version header, the one error shape, and the conformance suite any client-tools server runs against its own router — the same binary test answers for the mock and the daemon |
| `rhema-tools-common` | The client tools' shared plumbing: connection config from `RHEMA_URL`/`RHEMA_TOKEN`, the one HTTP call carrying the bearer and the contract header, and the answer-or-error-shape print with the exit code — no client reimplements the three moves, so all clients fail identically |
| `notify` | The one built-in client tool: desktop notifications through `notify-rust` (Linux/macOS/Windows), on the shared plumbing, so it speaks the same bearer + header envelope the suite verifies |
| `rhema-claw` | The resident agent daemon (work in progress): one SQLite store of sessions, forge threads answered with coding-agent turns, calendar-feed reminders, a `coding_agent` tool that runs coding jobs off the chat, chat over HTTP; the claw's sessions run the bounded request view (`rhema-transcript`) by default, so a stashed session can grow forever without growing every step's cost |

## rhema-core

Here is a small example code using `rhema-core` to interact with a model:

```rust
use std::sync::Arc;

use rhema_core::{
    async_trait, run_turn, Agent, AgentError, ChatParams, Content, Event,
    EventSink, Message, MockProvider, ModelOutput, Role, Session, StopReason,
    ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec, Usage,
};

struct Echo;

#[async_trait]
impl ToolResolver for Echo {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new("echo", "Echo the input back", serde_json::json!({}))]
    }
    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success(&call.id, call.args.to_string())
    }
}

// A sink that prints every event; replace with your UI/log/channel.
struct Print;
impl EventSink for Print {
    fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
}

// Any ModelProvider implementation; MockProvider is the scripted offline
// one (feature `mock`). A real HTTP adapter is a rhema-provider-* crate.
fn provider() -> Arc<dyn rhema_core::ModelProvider> {
    Arc::new(MockProvider::new(vec![ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::Text("done".into())],
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]))
}

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    let mut agent = Agent::builder()
        .provider(provider())
        .resolver(Arc::new(Echo))
        .system("you are a helpful agent")
        .params(ChatParams::new("model-id")) // any id your provider accepts; the mock ignores it
        .build()?;
    let mut session = Session::default();
    let ctx = ToolContext::default();
    let input = Message::text(Role::User, "run the echo tool, then summarize");
    let output = run_turn(&mut agent, &mut session, input, &mut Print, &ctx, 8).await?;
    println!("{output:?}");
    Ok(())
}
```

Offline demo with zero network (the mock replays a scripted exchange):

```sh
cargo run -p rhema-core --features mock --example step_demo
```
