# Rhema v1 spec

> **Status:** v1, 2026-10-06. Supersedes the morning draft.
> **System-level companion:** `specs/rhema-framework-v1.md`
> **Primitive deep-dive:** `specs/resource-scoped-dispatch.md`
> **One-line promise:** a small, embeddable, *generic* step engine in Rust. No coding-agent assumptions, no dispatch, no I/O baked in.

---

## 0. Scope

Crate 1 is **the step engine plus its seams**. It runs *one exchange* with a model: build request → stream → append assistant message → resolve tool calls → append results. It does not loop, does not dispatch, does not persist, does not sandbox.

**In v1:** the step primitive, its types, a model seam, a tool seam, an event/streaming seam, a caller-owned `Session`, and a mock for offline tests.

**OUT of v1** (each is deliberately a *driver* or a later crate, not a core concern):
- ❌ The turn loop (`run_turn`), a thin provided driver; see §5.
- ❌ Sessions-as-storage, checkpointing, SQLite. The driver persists; see §6.
- ❌ RSD dispatch: `ResourceKey`, `KeyFn`, `ClassifyFn`, `Lease`, `Inbox`, `Revision`, concurrency, admission queue. Crate 2.
- ❌ Provider adapters / HTTP. The `ModelProvider` trait is the only seam.
- ❌ Tool *registry*, discovery, `checkpoint()`, approvals.
- ❌ MCP, plugins (dylib/WASM/process), parallel tools, compaction, retries, cost.

---

## 1. The two consumers, and the discipline

The crate has exactly two intended consumers, and they impose opposite pressures. Name both; build for one.

| | **Coding agent** (first) | **Autonomous RSD agent** (second) |
|---|---|---|
| Driver | turn loop: `for _ in 0..max { step; if EndTurn → done }` | dispatcher loop: `acquire(key) → step … poll_inbox → fold → step … release` |
| Session key | fixed: `interactive:main` | one per `ResourceKey` |
| Session lifetime | the open connection | persisted across leases, survives restarts |
| Needs from crate 1 | `step`, streaming, tools | **the same** |

**The discipline:** crate 1 is built for the coding agent. The RSD agent is a **falsification test on the seams**, never a second feature set. If crate 1 later supports an RSD driver *without adding anything to crate 1*, the seam is correct. If supporting it requires `Lease`/`Inbox`/`Revision`/`KeyFn` to enter crate 1, the boundary is wrong.

This is testable, not aspirational; see §7 test 5.

Corollary from RSD §10: these are not two mechanisms. The interactive loop *is* an RSD session with a fixed key. One notion of "session", one dispatcher contract (crate 2), the loop as its first permanent consumer.

---

## 2. Layout

```
rhema/
  Cargo.toml            # [workspace] members = ["crates/*"]
  rust-toolchain.toml
  SPEC.md               # this file
  specs/                # the two predecessor specs
  README.md
  .gitignore            # /target
  crates/
    rhema-core/         # <-- crate 1
      Cargo.toml
      src/
        lib.rs          # re-exports; #![forbid(unsafe_code)]
        message.rs      # Role, Content, Message
        session.rs      # Session (history + opaque driver blob)
        request.rs      # ModelRequest, ChatParams, ToolSpec
        output.rs       # ModelOutput, StopReason, Usage, Step
        provider.rs     # trait ModelProvider + ProviderError
        tool.rs         # trait ToolResolver + ToolCall/ToolResult + ToolContext
        event.rs        # Event enum + EventSink
        engine.rs       # Agent (step engine) + builder + `step()`
        mock.rs         # MockProvider (behind `mock` feature)
      tests/
        step_tool_call.rs
        driver_code_agent.rs
        driver_rsd_style.rs
      examples/
        step_demo.rs
```

Project codename **Rhema** (ῥῆμα, "the spoken word": words that do things). Crate 1 is **`rhema-core`**.

---

## 3. Public API

`serde`-friendly. `thiserror`. `#![forbid(unsafe_code)]`. Trait names align with the framework spec (`ModelProvider`, `Session`, `ToolSpec`): one vocabulary, no synonyms.

```rust
// message.rs
pub enum Role { System, User, Assistant, Tool }
pub enum Content {
    Text(String),
    ToolCall(ToolCall),               // { id, name, args: Value }
    ToolResult(ToolResult),           // { id, content, is_error }
}
pub struct Message { pub role: Role, pub content: Vec<Content> }

// session.rs — caller-owned; loop only touches `history`
pub struct Session {
    pub history: Vec<Message>,
    /// Driver-owned. Crate 1 never reads this.
    /// The coding driver may leave it empty; the RSD driver stores key/plan/
    /// artifact_ref here (or keeps its own table). Opaque on purpose.
    pub driver_state: serde_json::Value,
}

// request.rs
pub struct ToolSpec { pub name: String, pub description: String, pub parameters: serde_json::Value }
pub struct ChatParams { pub model: String, pub temperature: Option<f32>, pub max_tokens: Option<u32> }
pub struct ModelRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub params: ChatParams,
}

// output.rs
pub enum StopReason { EndTurn, ToolUse, MaxTokens, StopSequence, Other(String) }
pub struct Usage { pub input_tokens: u32, pub output_tokens: u32, pub cached_input_tokens: u32 }
pub struct ModelOutput { pub message: Message, pub stop_reason: StopReason, pub usage: Usage }
pub struct Step { pub output: ModelOutput }

// provider.rs — the model seam
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stream deltas to `sink`; return the assembled final output.
    async fn complete(&self, req: ModelRequest, sink: &mut dyn EventSink)
        -> Result<ModelOutput, ProviderError>;
}

// tool.rs — the capability seam (one primitive, not a registry)
#[async_trait::async_trait]
pub trait ToolResolver: Send + Sync {
    /// The tools offered to the model this step.
    fn specs(&self) -> Vec<ToolSpec>;
    /// Turn a model's call into a result. Errors become in-band results.
    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult;
}
pub struct ToolContext { /* cwd, run id, cancellation — per-run, driver-provided */ }

// event.rs
pub enum Event<'a> {
    StepStart { iteration: u32 },
    TextDelta(&'a str),
    ToolCallStart { id: &'a str, name: &'a str },
    ToolCallArgsDelta(&'a str),
    ToolResult { id: &'a str, content: &'a str, is_error: bool },
    StepEnd { usage: Usage, stop_reason: &'a StopReason },
}
pub trait EventSink { fn emit(&mut self, event: Event<'_>); }

// engine.rs
pub struct Agent { /* provider, resolver: Option<Arc<dyn ToolResolver>>, system, params */ }
impl Agent {
    pub fn builder() -> AgentBuilder;
    /// ONE exchange. Never loops. Appends to `session.history` in place.
    pub async fn step(&mut self, session: &mut Session, sink: &mut dyn EventSink)
        -> Result<Step, AgentError>;
}
```

---

## 4. `step()` semantics (exact)

```
emit StepStart
req  = ModelRequest { system, messages: session.history.clone(), tools: resolver.specs(), params }
out  = provider.complete(req, sink)?                       # streams deltas
session.history.push(out.message)                          # assistant message (may hold ToolCall(s))
if out.stop_reason == ToolUse:
    for call in out.message's ToolCall(s), in order:
        emit ToolCallStart
        result = resolver.resolve(call, ctx)               # errors → ToolResult{is_error:true}
        emit ToolResult
        session.history.push(Message{ role: Tool, content: [result] })
emit StepEnd
return Step { output: out }
```

- **No loop, no `max_iterations`, no `Final`.** Those belong to a driver.
- **One exchange, but tool calls are resolved within it.** A step that requested tools returns with the results already appended, so the next step sees them. This is the loop's half of tool-use, and it is the minimum that makes it an agent rather than a chat call.
- Tool errors never abort; they surface as in-band `is_error` the model can react to. Unknown tool name is an error result, not a panic.
- No resolver configured → `specs()` is empty; a `ToolUse` stop with no resolver appends an error result.

---

## 5. Drivers (not crate 1)

The provided turn loop is ~15 lines; ship it in core as a convenience or in a sibling crate, either is fine, it is not the primitive.

```rust
pub async fn run_turn(agent: &mut Agent, session: &mut Session, input: Message, sink: &mut dyn EventSink)
    -> Result<ModelOutput, AgentError> {
    session.history.push(input);
    for i in 0..max {
        let step = agent.step(session, sink).await?;
        if step.output.stop_reason != StopReason::ToolUse { return Ok(step.output); }
    }
    Err(AgentError::MaxIterations)
}
```

The RSD driver (crate 2) is the same shape with different scaffolding: `acquire()` loads a `Session` from storage, steps, `poll_inbox()` between steps and appends delivered items to `history`, `checkpoint()` after each step, `release()`, all using the RSD contract, none of it visible to crate 1.

---

## 6. The Session / persistence boundary

| Concern | Owner |
|---|---|
| `history` mutation during a step | **crate 1** |
| `driver_state` (plan, artifact_ref, key, revision) | driver |
| Persisting a `Session` | driver (SQLite, per framework spec §7) |
| Loading a `Session` on acquire | driver |
| Lease, inbox, revision, debounce | crate 2 (RSD) |

`Session` is plain data and fully serializable: no hidden engine state, no open stream, so a driver can checkpoint it at any step boundary. That property is the whole reason crate 1 can serve the RSD consumer later without growing.

---

## 7. How we know it works

1. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
2. **`step_tool_call.rs`**: `MockProvider` is scripted to call a tool then answer; one `step()` dispatches the tool once and appends both the assistant message and the `Tool` result; event order `StepStart, ToolCallStart, ToolCallArgsDelta*, ToolResult, StepEnd`.
3. **Failing tool** → `is_error:true`, no panic, `step()` still returns `Ok`.
4. **`driver_code_agent.rs`**: the turn loop drives `step()` to completion; asserts bounded iterations.
5. **`driver_rsd_style.rs`**, **the falsification test.** A fake dispatcher holds a `Session`, runs a step, appends a synthetic "inbox" item to `history` *between* steps (as a `User` message), runs the next step, and round-trips the `Session` through serde to prove it is checkpointable. If this test is expressible **without touching crate 1**, the seam holds. If it needs a new type in crate 1, the boundary is wrong; fix the boundary, don't add the type.
6. `examples/step_demo.rs` runs against `MockProvider`, zero network.
7. `#![forbid(unsafe_code)]`; public items documented.

---

## 8. Open decisions

- ~~**Crate name**~~, **decided:** `rhema-core` (project codename **Rhema**).
- **`driver_state` shape**: opaque `serde_json::Value` (recommended, crate 1 stays ignorant) vs typed/generic `Session<T>`.
- **Streaming**: sink callback (recommended) vs `Stream` return; sink adapter is trivial.
- **`run_turn` home**: core convenience vs sibling crate.
- **Provider naming**: `ModelProvider` aligned to the framework spec (chosen) vs shorter `Llm`.
- **Whether `ToolContext` is created by the driver or the engine**: driver, so permissions/cwd are fixed before the step.
- ~~**Where the adapters' shared catalog types live**~~, **decided:** `rhema-provider`, the adapter-family base crate: `ModelCard` (the normalized listing row), `Pricing`, and the `SseDecoder`. What varies per backend stays in the adapter; each owns its text-chat filter over its own ids and bakes its prices into the rows it hands out, so `list_models` only ever returns models a coding agent can drive. No adapter depends on another.
- ~~**How a provider runs an interactive login**~~, **decided:** the `InteractiveLogin` seam in `rhema-provider` (`login.rs`). An adapter declares the flow (device codes, polling, expiry, all inside the flow) and reports `LoginUpdate`s to a sink; a frontend renders the updates generically and persists the `Completed` credential, one opaque string, the same shape a static API key takes, into the profile field it maps the provider to. Cancellation is dropping the flow's task; the flow keeps no state a retry would trip over. First consumer: the GitHub Copilot device flow.

---

## 9. Follow-ups (not v1)

Crate 2: RSD `SessionManager` (dispatch, leases, inbox, revisions, SQLite). Then: `rhema-provider-anthropic`, `rhema-provider-openai`, `rhema-tui`, compaction, provider fixtures via cassettes.

Later arrivals: `rhema-acp` serves the Agent Client Protocol (the
coding agent as an editor subprocess; the third `CodingAgent`
consumer, after the TUI and the one-shot), a falsification pass on
the driver seams that demanded no core changes and one justified
approval-bridge addition (`repair_dangling_calls` shared with the
TUI). Structured file tools remain the gate for ACP's diff and
follow-along UX; the adapter already answers `session/request_permission`.
