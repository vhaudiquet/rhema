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
    Usage(Usage),                  // provider-emitted running tally, cumulative per call
    StepEnd { usage: Usage, stop_reason: &'a StopReason },
}
pub trait EventSink { fn emit(&mut self, event: Event<'_>); }

// engine.rs
pub struct Agent { /* provider, resolver: Option<Arc<dyn ToolResolver>>, system, params,
                      commit: Option<CommitHook> — checkpoint per stream, see §4 */ }
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
commit_hook(session)?            # the session as it stands is durable — the caller's input included
                                 # — before the request spends tokens on it
req  = ModelRequest { system, messages: session.history.clone(), tools: resolver.specs(), params }
out  = provider.complete(req, sink)?                       # streams deltas
session.history.push(out.message)                          # assistant message (may hold ToolCall(s))
if out.stop_reason == ToolUse:
    commit_hook(session)?        # commit-before-execute: durable intent, or no side effect
    for call in out.message's ToolCall(s), in order:
        emit ToolCallStart
        result = resolver.resolve_streaming(call, ctx, progress)  # errors → ToolResult{is_error:true}
        emit ToolResult                                    # progress rode the sink as ephemeral ToolOutputDelta
                                                           # status rode it as ephemeral ToolStatus
        session.history.push(Message{ role: Tool, content: [result] })
    commit_hook(session)?        # the stream's outcome durable before the step is announced done
emit StepEnd
return Step { output: out }
```

- **No loop, no `max_iterations`, no `Final`.** Those belong to a driver.
- **One exchange, but tool calls are resolved within it.** A step that requested tools returns with the results already appended, so the next step sees them. This is the loop's half of tool-use, and it is the minimum that makes it an agent rather than a chat call.
- **The hook checkpoints per stream.** The optional commit hook (`CommitHook`) fires at every stream boundary: at the step's start, before the request (the caller's input durable before tokens are spent on it); on a `ToolUse` stop, after the assistant message is appended and before the first call resolves — commit-before-execute; and at the step's end, after the results (or after the assistant message when no tools follow), before the step is announced done. An `Err` fails the step (`AgentError::Commit`) at the boundary it fired on. The middle fire is how a driver makes a crash inside a tool call resumable: the durable history ends with the committed intent, and resume re-runs calls whose tool declared `ReplaySafety::Safe` while reporting the rest interrupted. The outer fires are how a death mid-turn — mid-first-stream included, a final answer that never touches a tool included — leaves a resumable session at the last boundary instead of no record at all.
- Tool errors never abort; they surface as in-band `is_error` the model can react to. Unknown tool name is an error result, not a panic.
- No resolver configured → `specs()` is empty; a `ToolUse` stop with no resolver appends an error result.
- Tool progress (`ToolOutputDelta`) is ephemeral like reasoning: streamed around the resolver's optional progress callback, never appended to history — the full text arrives in the result. A streaming resolver may also report its own status (`ToolStatus`: the phase its inner work is in and, when it runs model calls, the context its current model call carries and the cumulative spend across them, plus the model id) through the same progress channel; the engine stamps the call's id on it and the sink gets it as ephemeral `ToolStatus`, replacing any earlier one for the call. Status is screen-facing, never history.

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
- ~~**Where image input lives**~~, **decided:** the message seam, not a crate: a `Content::Image` block (`media_type` in the four formats every target provider accepts, bare-base64 `data`) rides `Message` beside the tool blocks, so sessions checkpoint it and every consumer speaks it. Each provider maps the block onto its own wire shape — OpenAI-dialect `image_url` data URIs, Anthropic base64 sources, Gemini `inlineData` — with the mapping documented as its wire contract. Vision support is a tri-state on `ModelCard` (`None` = unknown: most listings carry no modality field, and a curated verdict beats a guessed one only where the claim has a source — claude ids, Z.ai's GLM rows, DeepSeek's text-only facts, OpenRouter's published `input_modalities`). Frontends stage images as chips; claw stores inbound bytes in the attachment store and replays ids, never megabytes, over SSE.
- ~~**How a command's output becomes an image the model sees**~~, **decided:** a provisioned bash
  function, not a native tool — images are shell data, so the surface is `curl -sL url | see` (or
  `see path.png`), piped into a `BASH_ENV`-sourced function the `vision` module of
  `rhema-agent-tools` provisions the same way the MCP wire provisions its stubs. The function execs
  the rhema binary's hidden `__vision` mode, which sniffs magic bytes, refuses anything over 5 MB
  (Anthropic's per-image ceiling, the strictest target), and stages the bytes under a 0700
  per-process dir; the shell tool drains that staging dir after every command and fills
  `ToolResult::images`, a core field, so the image rides the result it came from and checkpoints
  with it. Providers map the field onto their wire — Anthropic sends a `tool_result` content
  block array, text first; OpenAI-dialect and Gemini tool messages carry text only, so each image
  is named in a trailing note the model can act on. A session whose bash runs elsewhere (remote
  sandbox, client-executed) wires nothing, the MCP gate's rule: the executor cannot write where
  the drain reads. Two init files (this one and the MCP provision's) chain by literal path — each
  sources its predecessor once, `|| true` — never by an env var: the executor layers the env map
  onto every command, so an inherited var reaches every generated init in a chain and an outer
  one re-sources itself until the shell dies by signal. The MCP provision carries the same fix.
- ~~**How a hosted Anthropic-compatible endpoint is served**~~, **decided:** the same `AnthropicProvider` under a base override and bearer auth, not a new adapter: Z.ai's GLM Coding Plan (`ZAI_BASE_URL`) speaks the Messages dialect and only the header scheme differs (`with_bearer_auth`). It is its own provider kind (`zai`, `ZAI_API_KEY`) with an overridable stored route, and its traffic identifies honestly as `rhema` — never another client's mark; quota multipliers are a vendor conversation, not a header.
- ~~**How transient provider failures retry**~~, **decided:** the `Retrying` decorator in
  `rhema-provider` (`retry.rs`), wrapped around every turn provider the frontends assemble —
  the TUI's `Backend::provider()`, claw's backend `construct()`, and ACP's `provider_for()` —
  never inside `rhema-core`, whose concern list this stays off. A request that failed with a
  retryable error (`ProviderError::is_retryable`: transport timeout, network failure, or an
  HTTP 408/5xx carried structurally on `ProviderError::Api`; a 429 quota window deliberately
  does not retry) is re-issued with exponential backoff, three attempts at most. Two guards keep a retry honest: a stream that already emitted an event
  never retries (the caller saw the deltas; a second attempt would repeat them), and an
  exchange that "succeeds" empty — a provider that closed the stream before its first chunk —
  retries like any dead response and, if every attempt comes back empty, fails naming that
  fact instead of completing a turn nobody saw. Subagents ride a second, background-tuned
  ladder on top of the frontend's (SPEC §13): a job's caller is not sitting behind the wait,
  and a dead job loses every step it finished, so a job waits out an outage past the hour
  where a turn gives up after minutes.
- ~~**What a manual `/retry` re-sends**~~, **decided:** the failed request itself, not the
  prompt text again. `CodingAgent::retry_turn_with_cancel` runs the step loop with no input
  message on the history exactly as the failed turn left it — a first-request failure re-issues
  the identical request, a mid-turn failure re-issues the step that failed with the committed
  steps still in view — so nothing lands twice and no committed tool call re-executes behind
  the model's back. The TUI gates it on "the last finished turn errored and nothing ran
  since" (a submit, a cancel, a success, or a conversation swap clears the offer) and shows
  `/retry re-sends it` under the error strip; the natural flow after a rate limit is
  `/model` then `/retry` on the new model. `/retry` also restarts every transient-failed
  subagent job in place (SPEC §13), whether or not the turn offer is up — a job can die to
  an outage the parent turn survived, and nothing else would re-run it.
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

- ~~**Where a tool's static data lives**~~, **decided:** a `TOOL.yaml`
  manifest per tool folder, parsed by `rhema-autonomous-agent`'s
  `manifest` module: name, description, argument schema, replay
  classification, and a `run` binding. The first binding is
  `run: { builtin: <id> }` — the operation stays compiled Rust that the
  consumer registers under that id, and a `ManifestTool` marries the
  manifest's spec to it — so a tool's spec stops living in Rust string
  literals and one folder names one tool, the same shape the skill and
  subagent folders already use. A manifest that does not parse, or
  names no known builtin, is skipped (reported, not fatal), like an
  undescribed skill folder; a shipped set pins itself with a drift
  test. The format takes further `run` kinds without a migration —
  `exec` (spawn a command, args on stdin, stdout is the result) is the
  noted next one, which is what lets non-Rust authors add tools. The
  claw's task tools are the first consumers.

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

---

## 10. The cloud frontend (`rhema-cloudflare-code`)

The fourth `CodingAgent` consumer, and the first whose platform is not
a POSIX process: the coding agent as a Cloudflare Durable Object, one
instance per session id. The engine, the driver, and the toolkit are
unchanged — what the cloud changes is where things live, and the
seams that took the move are the record of why.

**Why a DO plus a Sandbox container.** The DO gives the session one
addressable instance with durable SQLite storage and a WebSocket
surface; the Sandbox (a container bound to the same session name)
gives `bash` a real Linux workspace that persists across turns and
idle sleeps. The agent DO is the single writer of the session row, so
the `rhema-rsd` `Store` contract — one record per key, the revision
the compare-and-swap token — lands on DO storage without a
dispatcher.

**Container eviction is the committed-but-unanswered call.** Files
survive idle sleeps; a full container eviction does not, and a command
in flight dies with the machine. That is exactly the case the engine's
revive machinery exists for: `bash` is `ReplaySafety::Unsafe`, so the
resume reports the call interrupted instead of re-running it, and the
model verifies state before retrying. The offline e2e scripts the same
shape host-side: commit lands, the command parks, the turn dies, the
resume reports interrupted. The workspace itself is a checkout, not a
store — the session re-seeds what its config names (a git clone, the
skill files), and the meta-table marker keeps a resumed DO from
re-cloning over a workspace the client has since changed.

**The approval model.** `Approval::approve` is synchronous, and every
existing frontend blocks inside it (the ACP adapter parks a worker
thread). workerd has no second thread: a native block would stall the
isolate and deadlock the socket the answer arrives on. So the cloud
gate asks *before* tool resolution, at an await point: `ClientGate` is
a resolver that shadows `bash` with the same spec, answers from the
per-session policy (`deny_all`, an allowlist, `allow_all`), and hands
undecided calls to the connected client over the WebSocket — deny on
timeout (120 s), deny with no client. The turn-level hook stays
`DenyAll`: the gate is the only path a call takes, and the fail-safe
default stands behind it.

**Turn driving, v1.** A turn runs inside the client's live WebSocket
connection; checkpoints land at the commit hook — before every side
effect — and after each turn, so any eviction resumes through the
existing revive path. The known refinement is the alarm-driven step
queue, which would let a turn outlive any request and checkpoint after
every step; v1 declines to fight the invocation limits with it until a
turn shape actually needs it.

**The wasm discipline, kept.** The crate is two halves with one rule:
the platform lives at the edges. The neutral half (`sandbox`, `shell`,
`gate`, `assembly`, the scripted `mock`) compiles and tests on any
host with no `worker`, no wasm-bindgen, no fetch — the offline e2e is
the scripted sandbox plus the scripted provider, and it is the same
behavior the DO wraps. The `worker` module holds every host binding:
the fetch transport (whose `!Send` JS stream is pumped into plain-data
chunks inside one local task — the seam stays `Send` because the JS
handles never leave the bridge), the `worker::Delay` retry sleeper,
the DO storage `Store`, the WebSocket pump, and the JSON shim client
for the sandbox container (whose runtime is JS-only; the shim is the
sanctioned ~90-line translation, not a fight with JS RPC).
`worker = "0.8"` is pinned deliberately: the crate is 0.x and the
`DurableObject`/`SqlStorage`/`WebSocket` shapes this module uses are
0.8's. The `#[durable_object]` macro emits `unsafe` internally, so the
crate carries `deny(missing_docs)` but not a crate-level
`forbid(unsafe_code)`: the exception is scoped to the one class module
and documented where it is declared. Secrets ride wrangler secrets,
never DO storage; profiles ride the DO's meta table, never the XDG
config store.

Non-goals, v1: one provider adapter (Anthropic); no memory tools and
no subagents (both need storage-backed discovery to be honest on a
host with no filesystem); no hibernating WebSockets (the live-request
turn drives v1; hibernation rides the alarm-driven queue).

## 11. The frontend family (`rhema-cloud-agent`, `rhema-vercel-agent`)

The second cloud frontend made the harness its own crate. The rule
the extraction encodes: a cloud frontend is platform glue over the
same three pieces — an assembly that touches no filesystem, approval
decisions that round-trip through the connected client, and `bash` in
someone else's machine — and the pieces are one crate
(`rhema-cloud-agent`) that no platform dependency enters. The
per-session `SessionConfig` (model, base prompt, approval policy,
workspace, seed, skills, step cap) moved with it: it was never
platform code. A frontend persists it wherever its platform keeps
durable bytes — the DO's meta table, a Postgres row keyed by session —
and hands it to the assembly.

The client wire protocol is the shared surface too: one small
serde-typed module in the harness (`wire`), pinned by tests to the
byte shapes the DO already spoke, so a client written against one
frontend talks to the other and a shape drift fails a test here, not
a debugging session there.

`rhema-vercel-agent` is the proof the extraction was real: an axum
server (a Vercel container runs native Rust, so no JS shim and no
`!Send` bridge), the Sandbox REST client behind its own seam so the
offline tests run without a token, and Postgres carrying the same
revision-guarded `Store` records the DO's SQLite does. The sync
`Store` contract stands: the engine's commit hook calls it from
inside the turn, and a turn's checkpoint parking its worker thread
for a local-pool round trip is the same trade the ACP adapter makes
parking one for the editor. Turn driving stays v1 (the live
connection's request); an alarm-driven queue would let turns outlive
connections and is the same future refinement on both platforms.

**On "coding" agent.** Nothing in the engine, the harness, or the
cloud frontends knows code from anything else: `bash` is a tool, the
seams are domain-neutral, and "coding" is the default toolset and
prompt assembled on top. A computer-use frontend — the agent driving
an arbitrary machine rather than a repo — is a different toolset and
prompt over these same seams, not a fork. The crate names keep
`coding-agent` for the assembled default; the layer below never
assumed it.

**On forking.** Conversation topology (branch a session, run a child
against the parent's prefix) waits for a product shape that wants it.
The record format would carry it: a session's parent pointer and the
messages' first-line prefix in the RSD record, the stores
key-structured and JSON-native, no backend rewrite — deferred, not
designed away.

## 12. The sandbox seam (`rhema-sandbox`) and the execution choice

The sandbox began as cloud furniture: the `Sandbox` seam lived in the
shared harness, and each frontend constructed its platform's client
directly — the DO its `SandboxDo`, the Vercel server its
`VercelSandbox` — while the terminal frontend had no path to any of it.
The extraction makes the seam a crate of its own and the choice a matter
of configuration. `rhema-sandbox` is the seam crate of a family, the
provider family's shape applied to machines: the trait and its types,
the `ShellSandbox` adapter onto the executor seam, the scripted mock,
and — the part already written but unnamed — the wire protocol,
promoted from the Cloudflare shim's JSON surface (`exec`/`put`/`get`/
`seed`, a token header, tail-capped streams). `HttpSandbox` speaks it
over plain HTTPS and `rhema-sandboxd` serves it from any host, one root
directory per session; a DO pointed at one needs no container binding at
all, and the shim becomes optional. Backend selection rides deployment
env (`SANDBOX_BACKEND`), not the per-session config: it is a deployment
decision, and a client-chosen sandbox URL would be an SSRF on a machine
that executes commands.

The same crate is what lets `rhema-code` choose where its `bash` runs:
`local | docker | ssh | http` in v1 — stored, remembered per workspace
beside the remembered profile, pickable in the TUI, probed for
availability. The choice is profile state like the provider and the
model: a profile switch re-resolves `bash`'s home from the profile
moving in and re-assembles it, so work and personal never share a
sandbox by accident. Backends sort into two families — fs-sharing (`FsRoot`
plus an exec prefix: local direct, bubblewrap, a bind-mounted container)
and fs-disjoint (copied containers, VMs, SSH, the protocol family) — and
a VM costs no client code at all: an image running `sandboxd` is
packaging. The falsification test from the provider family repeats
here: every new backend must land with zero trait changes. `local`
stays today's fast path; non-local kinds seed skills into the sandbox
while session history, credentials, and the TUI stay host-side, which
they already are. Said plainly: local and mount postures are process
hygiene, not security boundaries, and the gate stays installed in every
mode. Full design and commit plan: `specs/rhema-sandbox.md`.

## 13. Async subagents (`rhema-subagents`)

The `subagent` tool stops resolving inside the step. A call **starts**
the subagent in the background and returns a job id in-band
immediately; subagents run in parallel; a finished job is announced to
the model by an automatic notification, and the model fetches the
report by running the tool against the job id. Escape stops the main
session and never a running subagent.

**Ownership.** The seams do not move (§0, §1):

- `rhema-subagents` owns everything about jobs: the registry (ids,
  states, report cache), the spawn, the fetch, the per-job cancel
  flags, the concurrency cap, and the notification text. It is the
  tool resolver; nothing outside it knows a job exists. `rhema-core`
  grows nothing — no scheduling, no injection, no job types.
- The driver tier (`rhema-coding-agent`, and any other `run_turn`
  owner) owns the step loop and the steering: draining finished jobs
  at step boundaries and appending the notification to the history.
  It does so behind one inert seam — `CodingAgentConfig` grows
  `steering: Option<Arc<dyn Steering>>`, `Steering::drain() ->
  Vec<String>` — so the driver stays subagent-agnostic, the same
  reading as `extra_resolver` (`specs/rhema-subagents.md` §5);
  `rhema-subagents` implements the trait on the registry. The coding
  driver's loop is its own from this change on — the same ~15 lines
  as the provided `run_turn` plus the two drain points; the provided
  one remains the no-steering convenience. A driver without a
  steering source behaves exactly as today.
- The engine (`Agent::step`) is untouched. Injecting between steps is
  a driver act, the same act the RSD driver already performs with its
  inbox (§5, §7 test 5); the second-consumer discipline is why no
  notification type enters crate 1.

**The tool's two shapes.** One tool, one spec, an optional third
argument:

```
subagent(name, prompt)   → starts; the result carries the job id at once
subagent(id)             → fetches the report of a started job
```

Same tool rather than a second one: one spec keeps the prompt's tool
list and the request's tools array from drifting, `replay_safety` is
per name so both shapes share one honest `Unsafe` (a fetch must not
launder the start's classification), and the description already
carries the catalog, so it carries the protocol too — calls start in
the background, a notification names the job when it finishes, the
report exists only in the fetch. Ids are the registry's, `job-<n>`
per agent. `name`+`prompt` and `id` are exclusive; both present, or
neither, is an in-band error.

**Steering — the notification.** A User-role message the driver
appends, one line per finished job:

    Subagent scout (job-3) has finished.
    Subagent reviewer (job-4) has finished with an error: hit its step budget of 256.

Chosen over the alternatives: a synthetic Tool-role result would need
a call id the model never emitted, and providers pair results to
calls by id — an unannounced id is rejected or dropped, and the
commit hook would be persisting an intent the model never decided. A
new engine event or seam fails §0/§1 outright. A User message is a
valid conversation for every provider (the Anthropic adapter already
merges User turns with tool-result turns), it is content the model
reads — not an ephemeral event — and it needs no synthetic id,
because the fetch is a real call the model itself decides, commits,
and can revive on.

Mechanics, exactly: the driver drains at two points — once at turn
start, before pushing the user input (consecutive User messages are
valid; anything finished while no turn was running is announced
here, whether or not it was announced before), and between steps,
after each `ToolUse` step and before the next. Drained lines
coalesce into one message; the append lands at a step boundary, so
the next commit hook and the driver's post-round checkpoint persist
it like any history. Notifications consume no step budget; the fetch
is an ordinary step. Within a turn each finish is announced once; at
each turn start every finished, unfetched, untaken job is announced
again — a line the frontend took for its idle prompt is retired and
never re-announced.
The drain is a query over registry state, never a mutation of
record: a crash between append and checkpoint only re-delivers.

Delivery when nothing runs — the frontend's half. A driver drains
only while a turn runs, so an interactive frontend owns the other
case: the registry hands out the pending lines again as a consuming
take, and the frontend runs them as the next turn's prompt. The take
marks the lines taken — announced, not fetched — so the driver's
turn-start drain cannot repeat what the prompt already said; the
report stays unfetched, so the fetch protocol works unchanged.
rhema-code does this on its redraw tick: idle, it joins the pending
lines into one prompt and starts the turn; mid-turn it takes only
the finish events (the transcript's notices) and leaves the lines to
the driver.

**Lifecycle.** `queued → running → finished(ok, report) |
finished(error, reason)`. A fetch marks the job fetched — its UI
line leaves, the report stays cached until the agent is dropped —
so a double-fetch returns the cached report, never a re-run:
subagents are `ReplaySafety::Unsafe`, and the fetch is a read of the
registry, not an execution.

- The start result names the job id and says a notification will
  follow; it never blocks on the job.
- A fetch of an ok job returns the cached report (capped at 50 KiB,
  tail kept, at report production). A fetch of a failed job returns
  `is_error` with the reason. A fetch of a running or queued job
  returns `is_error` "still running". An unknown id is an error
  naming it.
- A job the model never fetches keeps its report until the agent is
  dropped, and is re-announced at every turn start while unfetched
  and untaken — an interactive frontend's idle prompt also counts as
  an announcement. There is no forced delivery at turn end.

**Failure classes.** A job that ends without a report carries the
class its step error names, decided where the error is known
(`AgentError::is_retryable`, one question over
`ProviderError::is_retryable`): **transient** — a dead link, a
transport timeout, a struggling server, the failures the provider's
retry ladder waits out — or **permanent** — no report written, a
spent step budget, a rejected key. The class rides the finish, so
nothing ever classifies from the reason text.

**The background ladder.** The provider the inner agents run on is
the frontend's turn provider wrapped once more in `Retrying` inside
`rhema-subagents`, with its own ladders: the network one waits 1
minute, 5, 10, 30 (46 minutes of waits, each attempt re-walking the
frontend's ladder underneath, so the job's tolerance runs past the
hour), the server one 30 seconds and 2 minutes. A turn ladder gives
up after minutes because a user sits behind the wait; a job waits out
an outage because its caller is not behind it, and a dead job loses
every step it finished. The waits surface in the job's live line as a
`retrying model call (attempt n/m)` phase, so a job riding out an
outage reads as waiting, not hung.

**Restart.** A start arms its job with the spawn ingredients — the
definition, the prompt, the parent run id, the approval hook — and a
transient failure keeps them; `/retry` restarts every
transient-failed job in place, under its own id: the record returns
to queued with a fresh clock and cancel flag, the pending failure
announcements retire with the state they described (a model that read
one would delegate the same task beside the re-run), and the report
cache clears — a fetch reads "still running", and the re-run
announces its finish into the id the model already knows. A re-run
that fails transiently again keeps its ingredients and restarts
again. Permanent failures never restart — re-running them repeats the
failure at full cost — and a restart past the agent's drop is an
empty answer, never a spawn into a gone agent.

**Cancellation.** A job's `ToolContext.cancelled` is its own flag,
minted at spawn — never the parent turn's. Escape flips only the
turn's flag: the main session stops, running subagents do not. No
cancel-job path exists in v1 — the model cannot stop a job it
started, and the user stops one only by closing the agent; jobs are
bounded anyway (their step budget), and a stop verb over unattended
work is a later decision with its own approval question. Jobs are
bound to the agent's lifetime, not the turn's: a turn may end with
jobs running, they keep running, and the next turn's start announces
them. When the agent is dropped, running and queued jobs are
aborted and their reports are lost — the registry is in-memory, and
§6's plain-data session never carries it.

**Concurrency.** The registry runs at most 4 jobs at once (a
constructor knob, set like the nesting budget); a start beyond the
cap queues — the id is still minted and returned at once, and the
job begins when a slot frees. Queue, not reject: a rejection would
make a delegation fail on machine load the model cannot observe.
The cap counts top-level jobs only. A background job's own
`subagent` tool (when the nesting budget allows one) keeps today's
synchronous shape and runs to its report inside the job's step loop:
the notification path has exactly one steering consumer — the
driver's turn loop — and a nested job would have none.

**Approvals.** Background jobs inherit the turn's approval hook the
way foreground ones do — read off the parent shell at spawn,
installed on the job's inner shell — and the hook is captured at
spawn and held for the job's lifetime, so a gate firing after the
parent turn's end consults the turn's policy, never the fail-safe
reject. A gated call inside a background job raises the same modal,
blocking the job until it is answered, even while the parent
streams or after the parent turn has ended. Not deny-by-default:
background delegation would then be a recipe for every gated
command to fail whenever the user is not looking, and a blanket
allow would be a new unattended-execution path — exactly what the
hook exists to prevent. Denial answers in-band to the job, as to a
foreground one.

**UI surface.** One pinned live line per working job. The registry
publishes each job's status — the same `ToolStatus` vocabulary the
forwarder reports today (phase, spend, model) — on a subscription
`wire` hands back beside the config; the TUI pumps it beside the
engine events and keys a line by job id. A finished job's line
leaves at once — the strip pins working work only — and the
registry's finish feed hands the frontend one event per job, whose
line lands in the transcript as a muted notice, not a user bubble.
An idle session's prompt path reads as a user line instead: it is
the turn input the model received.

**Resume and replay.** Jobs are in-memory, so a crash mid-turn with
jobs running loses them. The committed history shows the start calls
answered with their job ids — nothing dangles, revive has nothing to
report. In the resumed session the registry is empty: a fetch of a
lost id returns `is_error` saying the job is gone, and the model
re-delegates. A start call that lost its result to the crash is
`Unsafe`: reported interrupted, never re-run.

## 14. Onboarding, bundled skills, and the no-gate chat (`rhema-code`)

The TUI's startup rule is: only the wizard gates, and only the first
time. `config.onboarded` carries the state — `None` (a config from
before the wizard, or no marker) reads as onboarded, `Some(false)`
reopens the wizard on the next start, `Some(true)` never shows it
again. `rhema onboard` forces it back on. Everything else that used to
gate — no usable backend, no stored model — opens the chat: the footer
names the missing piece in red, the submit box declines with the fix,
and `/login` or `/model` is one keystroke away. The wizard reuses the
settings' add-instance dialog inline (its validation is the model
fetch) and the palette list; steps live in `onboarding.rs`, and the
finish step is the only writer of `onboarded: true`, so Esc-ing out
halfway always reopens.

**Bundled skills.** `commit-message` and `unslop` ship in the binary
(`include_str!`) and seed into the global skills directory on start —
never overwriting an existing file, and skipping disabled names. The
config's `skills_disabled` set is the only switch: the wizard's skills
step and the settings Skills page flip it, the prompt filter
(`CodingAgentConfig::disabled_skills`, applied after collection and
after discovery alike) reads it, and the UIs show disabled rows greyed
rather than hidden. Re-enabling re-seeds a missing folder; a user's
folder of the same name shadows a bundled one like any other skill.
Discovery reads three tiers — the workspace's `.rhema/skills`, the
global config directory, and the cross-agent `~/.agents/skills`, the
highest tier shadowing the lower by name — so a skill in
either rhema directory hides an `.agents` copy of the same name,
never the reverse. Skills
carry an optional frontmatter `license` (plus a `NOTICE` file
where attribution must travel with copies), which the UIs render a
step dimmer than the description.

**Terminal graphics.** The wizard's rail draws the repo logo through
the kitty graphics protocol on kitty and Ghostty, placed outside the
ratatui frame (images composite above cells, so a frame never erases
one) and keyed on the rect the renderer restates each draw. Detection
is environment-based with a `RHEMA_NO_IMAGES` override; every other
terminal gets the wordmark.

**Shared chrome.** The full-screen menus draw their panels, selection
markers, and rules through `chrome.rs`, so the visual vocabulary
changes in one place — the coherence is in the calls, not in copies.
