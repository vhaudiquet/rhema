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
    /// Replay safety of one tool (default `Unsafe`): whether a committed
    /// call that lost its result to a crash may be re-executed on resume.
    fn replay_safety(&self, name: &str) -> ReplaySafety;
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
pub struct Agent { /* provider, resolver: Option<Arc<dyn ToolResolver>>, system, params,
                      commit: Option<CommitHook> — commit-before-execute, see §4 */ }
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
    commit_hook(session)?        # commit-before-execute: durable intent, or no side effect
    for call in out.message's ToolCall(s), in order:
        emit ToolCallStart
        result = resolver.resolve_streaming(call, ctx, progress)  # errors → ToolResult{is_error:true}
        emit ToolResult                                    # progress rode the sink as ephemeral ToolOutputDelta
        session.history.push(Message{ role: Tool, content: [result] })
emit StepEnd
return Step { output: out }
```

- **No loop, no `max_iterations`, no `Final`.** Those belong to a driver.
- **One exchange, but tool calls are resolved within it.** A step that requested tools returns with the results already appended, so the next step sees them. This is the loop's half of tool-use, and it is the minimum that makes it an agent rather than a chat call.
- **Commit-before-execute.** On a `ToolUse` stop the optional commit hook (`CommitHook`) runs after the assistant message is appended and before the first call resolves; an `Err` fails the step (`AgentError::Commit`) before any side effect. The hook is how a driver makes a crash inside a tool call resumable: the durable history ends with the committed intent, and resume re-runs calls whose tool declared `ReplaySafety::Safe` while reporting the rest interrupted. Without a hook, intent durability rides the driver's post-round checkpoints alone, and a crash mid-call loses the record that the call was decided. The post-round checkpoint (after the results) remains the driver's either way.
- Tool errors never abort; they surface as in-band `is_error` the model can react to. Unknown tool name is an error result, not a panic.
- No resolver configured → `specs()` is empty; a `ToolUse` stop with no resolver appends an error result.
- Tool progress (`ToolOutputDelta`) is ephemeral like reasoning: streamed around the resolver's optional progress callback, never appended to history — the full text arrives in the result.

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
- ~~**How a hosted Anthropic-compatible endpoint is served**~~, **decided:** the same `AnthropicProvider` under a base override and bearer auth, not a new adapter: Z.ai's GLM Coding Plan (`ZAI_BASE_URL`) speaks the Messages dialect and only the header scheme differs (`with_bearer_auth`). It is its own provider kind (`zai`, `ZAI_API_KEY`) with an overridable stored route, and its traffic identifies honestly as `rhema` — never another client's mark; quota multipliers are a vendor conversation, not a header.
- ~~**How transient provider failures retry**~~, **decided:** the `Retrying` decorator in
  `rhema-provider` (`retry.rs`), wrapped around every turn provider the frontends assemble —
  the TUI's `Backend::provider()`, claw's backend `construct()`, and ACP's `provider_for()` —
  never inside `rhema-core`, whose concern list this stays off. A request that failed with a
  retryable error (`ProviderError::is_retryable`: transport timeout, network failure, HTTP
  408/429/5xx as the adapters report them) is re-issued with exponential backoff, three
  attempts at most. Two guards keep a retry honest: a stream that already emitted an event
  never retries (the caller saw the deltas; a second attempt would repeat them), and an
  exchange that "succeeds" empty — a provider that closed the stream before its first chunk —
  retries like any dead response and, if every attempt comes back empty, fails naming that
  fact instead of completing a turn nobody saw.
- ~~**How a UI paste travels without flooding the session**~~, **decided:** embedded pastes in message text (`rhema-core::paste`). A frontend records a long paste as `U+FFFC`, payload, `U+FFFC` inside the user text it pushes to the session — the compact form is the session's record, so rebuilds and resumes render the same chip the live stream did. Expansion back to plain text rides the provider seam: the coding-agent driver wraps whatever `ModelProvider` it is built with, so the engine stays ignorant and the encoding never reaches an adapter. The engine's two-consumer discipline holds; a consumer that never mints marks is unaffected.
- ~~**Where forge support lives**~~, **decided:** an integration, not a
  framework component: `integrations/rhema-forge-integration` (the
  GitHub/Gitea/GitLab clients, the normalized event vocabulary, the RSD
  ingest path) sits outside the framework's `crates/` list and touches
  public API only. The driver role belongs to the resident tier's
  `Source`/`Driver` seams, so the integration ships no loop of its own
  (its pre-tier driver half, `ForgeBot`, was deleted); consumers such
  as the claw daemon compose it. It stays a workspace member — one
  gate, one lockfile — and can extract to its own repo at release.
- ~~**Where agent accounts live, and what a watched forge identity
  earns**~~, **decided:** `rhema-identity`, a peer of `rhema-memory` in
  the service tier: accounts with one secret each behind a
  use-never-read contract — `IdentityStore::secret` exists for *code*
  that acts with a credential, the crate ships no model surface, and
  consumers keep tools off it (claw's identity tools join the
  interactive chain only, so a forge comment cannot reach the
  accounts). A forge identity watched at creation feeds the
  integration's account-scoped notifications poll (`Forge::
  notifications`, the counterpart of the repo-scoped `poll`), and
  claw's triage turns each notification into a role-tagged thread
  event: the RSD key stays the thread — one session, one voice per
  thread — and the triaged role (reviewer, implementer) selects the
  dispatch's persona framing at handler time; mentions surface to the
  human instead of dispatching an agent.
- ~~**Where provider API keys live**~~, **decided:** the OS keychain
  behind a tiny `SecretStore` seam in `rhema-config` (`secrets`):
  get/set/delete of one opaque value per id, over the `keyring` crate
  (macOS Keychain, Windows Credential Manager, the Linux Secret
  Service). An instance holds either the literal `api_key` — the
  fallback where no keychain answers (containers, headless boxes) — or
  an opaque `secret_ref` naming its entry under the `rhema` service;
  resolution reads the environment, then the keychain, then the
  literal, and an unreachable store reads as "not in the store",
  never as a lost key. `Profile::place_key` (logins) and
  `Config::migrate_secrets` (frontend startup) prefer the chain and
  fall back to the file; forgetting an instance deletes its entry.
  `rhema-identity` is deliberately not the home: its accounts are the
  deployment's identities behind use-never-read, its SQLite backend
  stores secrets as plaintext columns, and a provider key is the
  user's per-profile credential, not an account. When identity grows
  its promised keychain backend, that backend consumes the `keyring`
  crate directly, as its SQLite backend consumes `rusqlite` — a
  shared mechanism, not a shared crate.
- ~~**How a running turn gets something only a human can give**~~,
  **decided:** the claw's consent round-trip — a claw mechanism, not
  an engine seam, keeping SPEC §1's "no approvals in crate 1" intact.
  A tool (`ask_secret`; the phone tool's control actions) inserts a
  consent row, puts a `consent_request` card on the session's stream,
  and blocks inside the tool call on a wake bridge; the chat surfaces
  answer through `POST /v1/consents/{id}` or deny with `DELETE`. A
  secret rides once, from the answer through the identity store — the
  row keeps only the draft without it, and the tool wakes with the
  account id, so use-never-read survives the round-trip. Denial and
  the fifteen-minute silence answer in-band, so a refused turn says so
  and moves on; expiry is by age at read, so a daemon restarted under
  a waiting tool stops offering a stale card. Using a stored
  credential is the mirror half: `identity_exec` (interactive chain
  only, like every accounts tool) runs one command with the secret in
  its environment plus a git credential helper, so the value reaches
  its destination — git, curl, a service CLI — without entering the
  transcript; the use-never-read rule bars echoes, not work.
- ~~**How the claw reaches the phone**~~, **decided:** the same shape
  turned outward, with one correction the fleet work earned. A topic
  report (`/v1/ingest`, `phone:<device>:<topic>`) is an observation:
  it lands in the phone tables and dispatches nothing, and the
  `phone` tool's `status`/`devices` actions read it without asking.
  The original shape dispatched every report like any resident key —
  conversation and state at once — on the premise that the agent
  should react to flips; in practice the transition rules bound
  volume, not value (a decile drop from 80 is not news), and each
  dispatched turn's reply auto-notifies the phone, so the premise
  cost a model call and a push notification per flip for reactions
  the agent could have made on demand with a status read. Deliberate
  reactions keep a home: a scheduled task reads status and notifies
  on the user's terms, and a bare `phone:<device>` key (a message on
  the device's resident key) still dispatches like any resident key.
  Acting is consent-gated end to end: an approved action
  joins a command queue the device polls and reports on
  (`/v1/phone/commands`), and the tool answers in-band — including
  "queued, not yet confirmed", the honest latency of a 15-minute
  WorkManager floor.
- **How the claw keeps the fleet honest when installs come and go**,
  **decided:** a device registry alongside the status tables, not a
  `DISTINCT` over them. A device id lives for one install — a
  reinstall, a cleared app data, a new phone each arrive under a fresh
  id, and an upgrade that regenerates one used to leave the old
  identity listed forever. `phone_devices` carries every identity that
  ever reported with its self-reported label (the client's
  manufacturer and model), first and last report; `observe` upserts it,
  so a reporting phone re-registers itself and a forgotten one that
  still reports comes back on its next report. `forget_device` retires
  the identity with its observations and its queued commands, and the
  same registry backs both management surfaces — `GET`/`DELETE
  /v1/phones` for the client's settings screen, the `phone` tool's
  `devices`/`forget` actions (forget consent-gated like the controls)
  for the agent.
- **Why no phone report dispatches**, **decided:** the topic key's
  shape is the whole rule. `phone:<device>:<topic>` is telemetry — it
  feeds the tables and the ingest door answers
  `{"ingested": "observed"}` — because a report's transitions bound
  the write volume but say nothing about whether a turn is worth it,
  and a turn per flip is a model call plus an auto-notified push per
  flip. The status question reads the snapshot; the deliberate
  reaction is a task. The client's own discipline (post only on
  change, a 300 m move for location) bounds the writes; dropping the
  dispatch bounds the tokens and the notification noise to zero.

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
