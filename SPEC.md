# Rhema design record

> **Status:** started as the v1 spec on 2026-10-06, extended as each
> component landed.
> **Primitive deep-dive:** `specs/resource-scoped-dispatch.md`
> **One-line promise:** a small, embeddable, *generic* step engine in Rust. No coding-agent assumptions, no dispatch, no I/O baked in.

Section numbers stay stable because code comments cite them. §3 (the v1
API sketch) and §9 (the follow-ups) were removed; rustdoc and the
released crates replaced them.

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

**The discipline:** crate 1 is built for the host agent. The RSD agent is a **falsification test on the seams**, never a second feature set. If crate 1 later supports an RSD driver *without adding anything to crate 1*, the seam is correct. If supporting it requires `Lease`/`Inbox`/`Revision`/`KeyFn` to enter crate 1, the boundary is wrong.

This is testable, not aspirational; see §7 test 5.

Corollary from RSD §10: these are not two mechanisms. The interactive loop *is* an RSD session with a fixed key. One notion of "session", one dispatcher contract (crate 2), the loop as its first permanent consumer.

---

## 2. Layout

The workspace members are the crates under `crates/`, each with its
own README. `crates/rhema-core/` is the step engine the rest builds
on. `crates/rhema-skills/` is skill discovery, the markdown folders
the cross-agent `.agents` convention and rhema's own tiers hold. The
workspace manifest is the list. The claw product (the daemon,
the forge and calendar integrations, the client tools, the web UI and
the Android client) lives in the `rhema-claw` repository and pulls
these crates from crates.io; the cloud product (the Durable Object
and the sandbox daemon image) lives in the `rhema-cloudflare-code`
repository under the same arrangement.

Project codename **Rhema** (ῥῆμα, "the spoken word": words that do things).

---

## 4. `step()` semantics (exact)

```
emit StepStart
commit_hook(session)?            # the session as it stands is durable, the caller's input included
                                 #, before the request spends tokens on it
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
- **The hook checkpoints per stream.** The optional commit hook (`CommitHook`) fires at every stream boundary: at the step's start, before the request (the caller's input durable before tokens are spent on it); on a `ToolUse` stop, after the assistant message is appended and before the first call resolves, commit-before-execute; and at the step's end, after the results (or after the assistant message when no tools follow), before the step is announced done. An `Err` fails the step (`AgentError::Commit`) at the boundary it fired on. The middle fire is how a driver makes a crash inside a tool call resumable: the durable history ends with the committed intent, and resume re-runs calls whose tool declared `ReplaySafety::Safe` while reporting the rest interrupted. The outer fires are how a death mid-turn, mid-first-stream included, a final answer that never touches a tool included, leaves a resumable session at the last boundary instead of no record at all.
- Tool errors never abort; they surface as in-band `is_error` the model can react to. Unknown tool name is an error result, not a panic.
- No resolver configured → `specs()` is empty; a `ToolUse` stop with no resolver appends an error result.
- Tool progress (`ToolOutputDelta`) is ephemeral like reasoning: streamed around the resolver's optional progress callback, never appended to history, the full text arrives in the result. A streaming resolver may also report its own status (`ToolStatus`: the phase its inner work is in and, when it runs model calls, the context its current model call carries and the cumulative spend across them, plus the model id) through the same progress channel; the engine stamps the call's id on it and the sink gets it as ephemeral `ToolStatus`, replacing any earlier one for the call. Status is screen-facing, never history.

---

## 5. Drivers (not crate 1)

The provided turn loop is ~15 lines; ship it in core as a convenience or in a sibling crate, either is fine, it is not the primitive.

```rust
pub async fn run_turn(
    agent: &mut Agent,
    session: &mut Session,
    input: Message,
    sink: &mut dyn EventSink,
    max_iterations: NonZeroU32,
) -> Result<ModelOutput, AgentError> {
    session.history.push(input);
    let mut remaining = max_iterations.get() - 1;
    let mut step = agent.step(session, sink).await?;
    while step.output.stop_reason == StopReason::ToolUse && remaining > 0 {
        remaining -= 1;
        step = agent.step(session, sink).await?;
    }
    Ok(step.output)
}
```

A turn still asking for tools at the cap returns that last output; the driver sees the stop reason and keeps the reply, usage, and any calls already resolved.

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

The frontends' reference store is `rhema-session-store-file`: the seam (`rhema-session-store`) owns the `SessionStore` trait, the validated id, the picker row, and the error; the backend owns the layout, the atomic fsynced write, and the privacy modes. The TUI, ACP, headless, and `rhema-host-agent`'s save/load helpers all go through it, and `rhema-session-store-sqlite` implements the same trait over one row per session. A frontend picks a backend at compile time; the seam pulls neither `rusqlite` nor `rhema-config`. See `specs/rhema-session-store.md`.

Requested work has a durable record too. `rhema-submissions` keeps one ledger under `Session.driver_state.submissions`, the state machine over it (`submit`, `start`, `settle`, `cancel`, `retry`, `reconcile`), and a seam a crate implements to teach it what its half of a crash means. The ledger rides the store's whole-session write, so the work's state and its status land in one record on every backend, and a resumed process can tell which requests finished, which are half done, and which never started. ACP records each turn there and reconciles on `session/load`; the TUI records one turn per prompt, settles it when the chain ends, and reconciles on `/resume`; `rhema-core` gains `Agent::resume_turn` so an interrupted turn continues instead of appending its input twice. See `specs/rhema-submissions.md`.

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

## 8. Decisions

- ~~**Crate name**~~, **decided:** `rhema-core` (project codename **Rhema**).
- ~~**`driver_state` shape**~~, **decided:** opaque `serde_json::Value`, so crate 1 stays ignorant of what a driver stores.
- ~~**Streaming**~~, **decided:** a sink callback; a `Stream` return would need a trivial adapter for no gain.
- ~~**`run_turn` home**~~, **decided:** `rhema_core`, beside `Agent::step`.
- ~~**Provider naming**~~, **decided:** `ModelProvider`, aligned to the framework spec.
- ~~**Whether `ToolContext` is created by the driver or the engine**~~, **decided:** the driver, so permissions and cwd are fixed before the step.
- ~~**Where the adapters' shared catalog types live**~~, **decided:** `rhema-provider`, the adapter-family base crate: `ModelCard` (the normalized listing row), `Pricing`, and the `SseDecoder`. What varies per backend stays in the adapter; each owns its text-chat filter over its own ids and bakes its prices into the rows it hands out, so `list_models` only ever returns models a coding agent can drive. No adapter depends on another.
- ~~**How a provider runs an interactive login**~~, **decided:** the `InteractiveLogin` seam in `rhema-provider` (`login.rs`). An adapter declares the flow (device codes, polling, expiry, all inside the flow) and reports `LoginUpdate`s to a sink; a frontend renders the updates generically and persists the `Completed` credential, one opaque string, the same shape a static API key takes, into the profile field it maps the provider to. Cancellation is dropping the flow's task; the flow keeps no state a retry would trip over. First consumer: the GitHub Copilot device flow. The ChatGPT adapter adds the browser authorization-code flow with PKCE on the same seam.
- ~~**Where image input lives**~~, **decided:** the message seam, not a crate: a `Content::Image` block (`media_type` in the four formats every target provider accepts, bare-base64 `data`) rides `Message` beside the tool blocks, so sessions checkpoint it and every consumer speaks it. Each provider maps the block onto its own wire shape, OpenAI-dialect `image_url` data URIs, Anthropic base64 sources, Gemini `inlineData`, with the mapping documented as its wire contract. Vision support is a tri-state on `ModelCard` (`None` = unknown: most listings carry no modality field, and a curated verdict beats a guessed one only where the claim has a source, claude ids, Z.ai's GLM rows, DeepSeek's text-only facts, OpenRouter's published `input_modalities`). Frontends stage images as chips; claw stores inbound bytes in the attachment store and replays ids, never megabytes, over SSE.
- ~~**How a command's output becomes an image the model sees**~~, **decided:** a provisioned bash
  function, not a native tool, images are shell data, so the command is `curl -sL url | see` (or
  `see path.png`), piped into a `BASH_ENV`-sourced function the `vision` module of
  `rhema-bash` provisions the same way the MCP wire provisions its stubs. The function execs
  the rhema binary's hidden `__vision` mode, which sniffs magic bytes, refuses anything over 5 MB
  (Anthropic's per-image ceiling, the strictest target), and stages the bytes under a 0700
  per-process dir; the shell tool drains that staging dir after every command and fills
  `ToolResult::images`, a core field, so the image rides the result it came from and checkpoints
  with it. Providers map the field onto their wire, Anthropic sends a `tool_result` content
  block array, text first; OpenAI-dialect and Gemini tool messages carry text only, so the images follow
  in the nearest image-capable spot: a `user` turn naming the call on the OpenAI dialect, `inlineData`
  parts in the same turn on Gemini, the shapes a pasted image already rides. A Responses-API mode
  carries them inside the `function_call_output` itself, which that API accepts as content parts. A session whose bash runs elsewhere (remote
  sandbox, client-executed) wires nothing, the MCP gate's rule: the executor cannot write where
  the drain reads. Two init files (this one and the MCP provision's) chain by literal path, each
  sources its predecessor once, `|| true`, never by an env var: the executor layers the env map
  onto every command, so an inherited var reaches every generated init in a chain and an outer
  one re-sources itself until the shell dies by signal. The MCP provision carries the same fix.
- ~~**Which request shape a custom OpenAI-compatible endpoint speaks**~~, **decided:** the `openai` kind defaults by route, Responses at OpenAI itself, chat completions under a stored base override, and a per-instance `api` field (`chat` / `responses`, persisted as `ProviderApi`) overrides the rule, because a user who knows their endpoint serves Responses should not be told otherwise. The reference frontend asks it on the add-instance flow's choice stage, inserted between the base-URL and key prompts for the custom-endpoint row only (chat highlighted: enter-without-looking keeps the route rule's answer); the ACP resolver and claw's params honor the same field; every other kind's dialect stays its endpoint's own decision (deepseek/opencode/openrouter always Responses, llama.cpp chat). Unknown spellings fall back to the rule.
- ~~**How a hosted Anthropic-compatible endpoint is served**~~, **decided:** the same `AnthropicProvider` under a base override and bearer auth, not a new adapter: Z.ai's GLM Coding Plan (`ZAI_BASE_URL`) speaks the Messages dialect and only the header scheme differs (`with_bearer_auth`). It is its own provider kind (`zai`, `ZAI_API_KEY`) with an overridable stored route, and its traffic identifies honestly as `rhema`, never another client's mark; quota multipliers are a vendor conversation, not a header.
- ~~**Where the name-to-adapter construction lives**~~, **decided:**
  `rhema-provider-registry`, one `build` per rhema-config `PROVIDERS`
  spelling: the kind plus whatever the caller resolved (a key, the
  llama.cpp server address, a stored route override) in, the turn
  provider and the catalog listing out, both trait objects. The
  recipes the frontends used to each spell, deepseek is the generic
  adapter at DeepSeek's endpoint, zai is the Anthropic adapter at
  Z.ai's route under bearer auth, a stored route replaces a kind's
  default while a blank reads as unset, live in the registry (the
  base constants and named route constructors stay on the adapters
  themselves); what stays with the frontend is policy: credential
  resolution, the retry wrapper, the default model. The catalog seam
  behind it is `ModelListing` in `rhema-provider`, on every adapter,
  so code holding only a name can list the fleet's models, no
  concrete type, no per-frontend boxing. The listing deliberately
  stays off `ModelProvider`'s turn seam and off the retry: a catalog
  fetch answers or fails, no ladder.
- ~~**How transient provider failures retry**~~, **decided:** the `Retrying` decorator in
  `rhema-provider` (`retry.rs`), wrapped around every turn provider the frontends assemble,
  the TUI's `Backend::provider()`, claw's backend `construct()`, and ACP's `provider_for()`,
  never inside `rhema-core`, whose concern list this stays off. A request that failed with a
  retryable error (`ProviderError::is_retryable`: transport timeout, network failure, or an
  HTTP 408/5xx carried structurally on `ProviderError::Api`; a 429 quota window deliberately
  does not retry, except a `ProviderError::RateLimited` whose provider-named wait is at most
  `MAX_RATE_LIMIT_WAIT`, 90 s: a per-minute token or request window, waited out for exactly
  the named reset, four times at most; the OpenAI-compatible adapter reads the reset off
  `retry-after-ms`, `retry-after`, the body's "try again in", or `x-ratelimit-reset-*`) is re-issued with exponential backoff, three attempts at most. Two guards keep a retry honest: a stream that already emitted an event
  never retries (the caller saw the deltas; a second attempt would repeat them), and an
  exchange that "succeeds" empty, a provider that closed the stream before its first chunk,
  retries like any dead response and, if every attempt comes back empty, fails naming that
  fact instead of completing a turn nobody saw. Subagents ride a second, background-tuned
  ladder on top of the frontend's (SPEC §13): a job's caller is not sitting behind the wait,
  and a dead job loses every step it finished, so a job waits out an outage past the hour
  where a turn gives up after minutes.
- ~~**What a manual `/retry` re-sends**~~, **decided:** the failed request itself, not the
  prompt text again. `HostAgent::retry_turn_with_cancel` runs the step loop with no input
  message on the history exactly as the failed turn left it, a first-request failure re-issues
  the identical request, a mid-turn failure re-issues the step that failed with the committed
  steps still in view, so nothing lands twice and no committed tool call re-executes behind
  the model's back. The TUI gates it on "the last finished turn errored and nothing ran
  since" (a submit, a cancel, a success, or a conversation swap clears the offer) and shows
  `/retry re-sends it` under the error strip; the natural flow after a rate limit is
  `/model` then `/retry` on the new model. `/retry` also restarts every transient-failed
  subagent job in place (SPEC §13), whether or not the turn offer is up, a job can die to
  an outage the parent turn survived, and nothing else would re-run it.
- ~~**How a UI paste travels without flooding the session**~~, **decided:** plain text plus a consumer-owned annotation. The prompt travels with each payload in place, exactly what the model reads; the frontend records the byte range and line count of every paste in `Message.annotations` under its own key (`rhema-code`), so a rebuild draws the chip. The record lives in the frontend (`rhema-code`'s `paste` module), not in rhema-core: the TUI minted the encoding, no other frontend ever wrote a mark, and core keeps only the generic annotations seam. The field is serde-defaulted, skipped when empty, and no engine code, provider adapter, or summarizer reads it. Annotations are the general seam for display-only facts a frontend needs to survive a checkpoint. The older embedded form (`U+FFFC`, payload, `U+FFFC`) is read once, on the frontend's resume: `paste::migrate` rewrites it into plain text plus spans right after the session file loads, and the encoding retires with the sessions that carry it.
- ~~**Where forge support lives**~~, **decided:** an integration, not a
  framework component: `integrations/rhema-forge-integration` (the
  GitHub/Gitea/GitLab clients, the normalized event vocabulary, the RSD
  ingest path) sits outside the framework's `crates/` list and touches
  public API only. The driver role belongs to the resident tier's
  `Source`/`Driver` seams, so the integration ships no loop of its own
  (its pre-tier driver half, `ForgeBot`, was deleted); consumers such
  as the claw daemon compose it. It stays a workspace member, one
  gate, one lockfile, and can extract to its own repo at release.
- ~~**Where agent accounts live, and what a watched forge identity
  earns**~~, **decided:** `rhema-identity`, a peer of `rhema-memory` in
  the service tier: accounts with one secret each behind a
  use-never-read contract, `IdentityStore::secret` exists for *code*
  that acts with a credential, the crate ships no model calls, and
  consumers keep tools off it (claw's identity tools join the
  interactive chain only, so a forge comment cannot reach the
  accounts). A forge identity watched at creation feeds the
  integration's account-scoped notifications poll (`Forge::
  notifications`, the counterpart of the repo-scoped `poll`), and
  claw's triage turns each notification into a role-tagged thread
  event: the RSD key stays the thread, one session, one voice per
  thread, and the triaged role (reviewer, implementer) selects the
  dispatch's persona framing at handler time; mentions surface to the
  human instead of dispatching an agent.
- ~~**Where the rhema family's directories live**~~, **decided:** the
  products of the rhema family share the XDG bases and nest below the
  family name, the reference frontend at
  `<config-base>/rhema`, the claw below it (`rhema/claw.yaml`,
  `state/rhema/claw`), the ACP adapter's session cache below that
  (`cache/rhema/acp-sessions`). `rhema-config`'s `Home` resolves all
  four roots once (`discover`) and hands them down; nothing downstream
  reads an XDG variable. A frontend from outside the family relocates
  the whole set instead (`Home::relocated`): its config base as given,
  and its own name, the base's final component, dot stripped, under
  the state and cache bases, so godcode's artifacts never land under
  `rhema/`. The credential separation matches: the keychain namespace
  is per frontend, the directory nesting is per family, and both are
  decisions a frontend states once at its construction site.
- ~~**Where provider API keys live**~~, **decided:** the OS keychain
  behind a tiny `SecretStore` seam in `rhema-config` (`secrets`):
  get/set/delete of one opaque value per id, over the `keyring` crate
  (macOS Keychain, Windows Credential Manager, the Linux Secret
  Service). An instance holds either the literal `api_key`, the
  fallback where no keychain answers (containers, headless boxes), or
  an opaque `secret_ref` naming its entry under the frontend's
  keychain namespace (the reference frontend's is `rhema`; a second
  frontend names its own, so two frontends sharing a machine never
  share credentials by accident); resolution reads the environment,
  then the keychain, then the literal, and an unreachable store reads
  as "not in the store", never as a lost key. `Profile::place_key`
  (logins) and `Settings::migrate_secrets` (frontend startup) prefer
  the chain and fall back to the file; forgetting an instance deletes
  its entry.
  `rhema-identity` is deliberately not the home: its accounts are the
  deployment's identities behind use-never-read, its SQLite backend
  stores secrets as plaintext columns, and a provider key is the
  user's per-profile credential, not an account. When identity grows
  its promised keychain backend, that backend consumes the `keyring`
  crate directly, as its SQLite backend consumes `rusqlite`, a
  shared mechanism, not a shared crate.
- ~~**How a running turn gets something only a human can give**~~,
  **decided:** the claw's consent round-trip, a claw mechanism, not
  an engine seam, keeping SPEC §1's "no approvals in crate 1" intact.
  A tool (`ask_secret`; the phone tool's control actions) inserts a
  consent row, puts a `consent_request` card on the session's stream,
  and blocks inside the tool call on a wake bridge; the chat surfaces
  answer through `POST /v1/consents/{id}` or deny with `DELETE`. A
  secret rides once, from the answer through the identity store, the
  row keeps only the draft without it, and the tool wakes with the
  account id, so use-never-read survives the round-trip. Denial and
  the fifteen-minute silence answer in-band, so a refused turn says so
  and moves on; expiry is by age at read, so a daemon restarted under
  a waiting tool stops offering a stale card. Using a stored
  credential is the mirror half: `identity_exec` (interactive chain
  only, like every accounts tool) runs one command with the secret in
  its environment plus a git credential helper, so the value reaches
  its destination, git, curl, a service CLI, without entering the
  transcript; the use-never-read rule bars echoes, not work.
- ~~**How the claw reaches the phone**~~, **decided:** the same shape
  turned outward, with one correction the fleet work earned. A topic
  report (`/v1/ingest`, `phone:<device>:<topic>`) is an observation:
  it lands in the phone tables and dispatches nothing, and the
  `phone` tool's `status`/`devices` actions read it without asking.
  The original shape dispatched every report like any resident key,
  conversation and state at once, on the premise that the agent
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
  (`/v1/phone/commands`), and the tool answers in-band, including
  "queued, not yet confirmed", the honest latency of a 15-minute
  WorkManager floor.
- **How the claw keeps the fleet honest when installs come and go**,
  **decided:** a device registry alongside the status tables, not a
  `DISTINCT` over them. A device id lives for one install, a
  reinstall, a cleared app data, a new phone each arrive under a fresh
  id, and an upgrade that regenerates one used to leave the old
  identity listed forever. `phone_devices` carries every identity that
  ever reported with its self-reported label (the client's
  manufacturer and model), first and last report; `observe` upserts it,
  so a reporting phone re-registers itself and a forgotten one that
  still reports comes back on its next report. `forget_device` retires
  the identity with its observations and its queued commands, and the
  same registry backs both management surfaces, `GET`/`DELETE
  /v1/phones` for the client's settings screen, the `phone` tool's
  `devices`/`forget` actions (forget consent-gated like the controls)
  for the agent.
- **Why no phone report dispatches**, **decided:** the topic key's
  shape is the whole rule. `phone:<device>:<topic>` is telemetry, it
  feeds the tables and the ingest door answers
  `{"ingested": "observed"}`, because a report's transitions bound
  the write volume but say nothing about whether a turn is worth it,
  and a turn per flip is a model call plus an auto-notified push per
  flip. The status question reads the snapshot; the deliberate
  reaction is a task. The client's own discipline (post only on
  change, a 300 m move for location) bounds the writes; dropping the
  dispatch bounds the tokens and the notification noise to zero.

- ~~**Where a tool's static data lives**~~, **decided:** a `TOOL.yaml`
  manifest per tool folder: name, description, argument schema, replay
  classification, and a `run` binding. The first binding is
  `run: { builtin: <id> }`, the operation stays compiled Rust that the
  consumer registers under that id, so a tool's spec stops living in
  Rust string literals and one folder names one tool, the same shape
  the skill and subagent folders already use. A manifest that does not
  parse, or names no known builtin, is skipped (reported, not fatal),
  like an undescribed skill folder; a shipped set pins itself with a
  drift test. The format takes further `run` kinds without a
  migration, but the noted `exec` kind (spawn a command, args on
  stdin, stdout is the result) will not be built: user-written script
  tools are an MCP server's job, `rhema-mcp` already wires those, and
  a second process protocol would be a small reinvention of it. Home:
  the consumer, not the framework. The 0.5.0 review cut the whole
  resident tool seam (`Allowlist`, the `Tool` trait, the registry and
  its filtered resolver, `ResolverChain`, the manifest loader) from
  `rhema-autonomous-agent`: the 0.4.0 consumer audit showed the loop
  half imports none of it, every allowlist in the wild is permissive,
  every tool arrives as manifest plus operation, and the registry is
  built, filled, and flattened into a resolver in the same breath at
  every call site, packaging for one consumer, not framework API. The
  claw owns its tooling from here, claw-local per family: a
  `TOOL.yaml` loader over its embedded manifests beside the registered
  builtins, or inline specs next to the operation, whichever the
  family prefers. Custom native tools are one `rhema_core`
  `ToolResolver` impl, no framework machinery; the resident tier keeps
  only the `Source`/`Driver` loop.

---

## 10. The cloud frontend (`rhema-cloudflare-code`)

The Durable Object frontend and the sandbox daemon image live in the
`rhema-cloudflare-code` repository, with the deploy assets (wrangler
config, the JS sandbox shim) and the DO's design record. This
section's full text moved with it. What stays here is the harness it
assembles from (§11) and the seam its `bash` runs through (§12), both
published to crates.io and consumed at the workspace-pinned version.

## 11. The frontend family (`rhema-cloud-agent`)

A second cloud frontend made the harness its own crate. The rule
the extraction encodes: a cloud frontend is platform glue over the
same three pieces, an assembly that touches no filesystem, approval
decisions that round-trip through the connected client, and `bash` in
someone else's machine, and the pieces are one crate
(`rhema-cloud-agent`) that no platform dependency enters. The
per-session `SessionConfig` (model, base prompt, approval policy,
workspace, seed, skills, step cap) moved with it: it was never
platform code. A frontend persists it wherever its platform keeps
durable bytes, the DO's meta table, a Postgres row keyed by session,
and hands it to the assembly.

The client wire protocol is shared too: one small
serde-typed module in the harness (`wire`), pinned by tests to the
byte shapes the DO already spoke, so a client written against one
frontend talks to any other and a shape drift fails a test here, not
a debugging session there.

The crate publishes to crates.io; its one consumer, the Durable Object,
lives in the `rhema-cloudflare-code` repository and pins it there, the
same arrangement the claw keeps with `rhema-autonomous-agent`. A second
cloud frontend on Vercel (an axum server, since removed) proved
the extraction was real. What it proved still stands: the sync `Store`
contract is workable, because the engine's commit hook calls it from
inside the turn, and a turn's checkpoint parking its worker thread for a
local-pool round trip is the same trade the ACP adapter makes parking
one for the editor. Turn driving stays v1 (the live connection's
request); an alarm-driven queue would let turns outlive connections and
is the same future refinement.

**On "host" agent.** Nothing in the engine or the harness knows code
from anything else: `bash` is a tool, the seams are domain-neutral.
The driver crate is `rhema-host-agent`: the agent whose tools operate
a host. "Coding" is a toolset and persona assembled on top,
`rhema-code` being that assembly; the driver crate never assumed it.

**On forking.** Conversation topology (branch a session, run a child
against the parent's prefix) waits for a product shape that wants it.
The record format would carry it: a session's parent pointer and the
messages' first-line prefix in the RSD record, the stores
key-structured and JSON-native, no backend rewrite, deferred, not
designed away.

## 12. The sandbox seam (`rhema-sandbox`) and the execution choice

The sandbox began as cloud furniture: the `Sandbox` seam lived in the
shared harness, and each frontend constructed its platform's client
directly, the DO its `SandboxDo`, the Vercel server its
`VercelSandbox`, while the terminal frontend had no path to any of it.
The extraction makes the seam a crate of its own and the choice a matter
of configuration. `rhema-sandbox` is the seam crate of a family, the
provider family's shape applied to machines: the trait and its types,
the `ShellSandbox` adapter onto the executor seam, the scripted mock,
and, the part already written but unnamed, the wire protocol,
promoted from the Cloudflare shim's JSON commands (`exec`/`put`/`get`/
`seed`, a token header, tail-capped streams). `HttpSandbox` speaks it
over plain HTTPS and `rhema-sandboxd` serves it from any host, one root
directory per session; a DO pointed at one needs no container binding at
all, and the shim becomes optional. Backend selection rides deployment
env (`SANDBOX_BACKEND`), not the per-session config: it is a deployment
decision, and a client-chosen sandbox URL would be an SSRF on a machine
that executes commands.

The same crate is what lets `rhema-code` choose where its `bash` runs:
`local | docker | ssh | http` in v1, stored, remembered per workspace
beside the remembered profile, pickable in the TUI, probed for
availability. The choice is profile state like the provider and the
model: a profile switch re-resolves `bash`'s home from the profile
moving in and re-assembles it, so work and personal never share a
sandbox by accident. Backends sort into two families, fs-sharing (`FsRoot`
plus an exec prefix: local direct, bubblewrap, a bind-mounted container)
and fs-disjoint (copied containers, VMs, SSH, the protocol family), and
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
  grows nothing, no scheduling, no injection, no job types.
- The driver tier (`rhema-host-agent`, and any other `run_turn`
  owner) owns the step loop and the steering: draining finished jobs
  at step boundaries and appending the notification to the history.
  It does so behind one inert seam, `HostAgentConfig` grows
  `steering: Option<Arc<dyn Steering>>`, `Steering::drain() ->
  Vec<String>`, so the driver stays subagent-agnostic, the same
  reading as `extra_resolver` (`specs/rhema-subagents.md` §5);
  `rhema-subagents` implements the trait on the registry. The coding
  driver's loop is its own from this change on, the same ~15 lines
  as the provided `run_turn` plus the two drain points; the provided
  one remains the no-steering convenience. A driver without a
  steering source behaves exactly as today.
- The engine (`Agent::step`) is untouched. Injecting between steps is
  a driver act, the same act the RSD driver already performs with its
  inbox (§5, §7 test 5); the second-consumer discipline is why no
  notification type enters crate 1.

**The tool's two shapes.** One tool, one spec, optional arguments
alongside (`id` to fetch, `model` to move the run, see the crate spec
§4, *Where a run rides*):

```
subagent(name, prompt)   → starts; the result carries the job id at once
subagent(id)             → fetches the report of a started job
```

Same tool rather than a second one: one spec keeps the prompt's tool
list and the request's tools array from drifting, `replay_safety` is
per name so both shapes share one honest `Unsafe` (a fetch must not
launder the start's classification), and the description already
carries the catalog, so it carries the protocol too, calls start in
the background, a notification names the job when it finishes, the
report exists only in the fetch. Ids are the registry's, `job-<n>`
per agent. `name`+`prompt` and `id` are exclusive; both present, or
neither, is an in-band error.

**Steering, the notification.** A User-role message the driver
appends, one line per finished job:

    Subagent scout (job-3) has finished.
    Subagent reviewer (job-4) has finished with an error: hit its step budget of 256.

Chosen over the alternatives: a synthetic Tool-role result would need
a call id the model never emitted, and providers pair results to
calls by id, an unannounced id is rejected or dropped, and the
commit hook would be persisting an intent the model never decided. A
new engine event or seam fails §0/§1 outright. A User message is a
valid conversation for every provider (the Anthropic adapter already
merges User turns with tool-result turns), it is content the model
reads, not an ephemeral event, and it needs no synthetic id,
because the fetch is a real call the model itself decides, commits,
and can revive on.

Mechanics, exactly: the driver drains at two points, once at turn
start, before pushing the user input (consecutive User messages are
valid; anything finished while no turn was running is announced
here, whether or not it was announced before), and between steps,
after each `ToolUse` step and before the next. Drained lines
coalesce into one message; the append lands at a step boundary, so
the next commit hook and the driver's post-round checkpoint persist
it like any history. Notifications consume no step budget; the fetch
is an ordinary step. Within a turn each finish is announced once; at
each turn start every finished, unfetched, untaken job is announced
again, a line the frontend took for its idle prompt is retired and
never re-announced.
The drain is a query over registry state, never a mutation of
record: a crash between append and checkpoint only re-delivers.

Delivery when nothing runs, the frontend's half. A driver drains
only while a turn runs, so an interactive frontend owns the other
case: the registry hands out the pending lines again as a consuming
take, and the frontend runs them as the next turn's prompt. The take
marks the lines taken, announced, not fetched, so the driver's
turn-start drain cannot repeat what the prompt already said; the
report stays unfetched, so the fetch protocol works unchanged.
rhema-code does this on its redraw tick: idle, it joins the pending
lines into one prompt, records each as its own notification card,
the transcript's record of the delivery, never styled as user input,
though the model-facing history is a User message either way, and
starts the turn; mid-turn it takes only the finish events (their
cards, at detection) and leaves the lines to the driver.

**Lifecycle.** `queued → running → finished(ok, report) |
finished(error, reason)`. A fetch marks the job fetched, its UI
line leaves, the report stays cached until the agent is dropped,
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
  and untaken, an interactive frontend's idle prompt also counts as
  an announcement. There is no forced delivery at turn end.

**Failure classes.** A job that ends without a report carries the
class its step error names, decided where the error is known
(`AgentError::is_retryable`, one question over
`ProviderError::is_retryable`): **transient**: a dead link, a
transport timeout, a struggling server, the failures the provider's
retry ladder waits out, or **permanent**: no report written, a
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

**Restart.** A start arms its job with the spawn ingredients, the
definition, the prompt, the parent run id, the approval hook, and a
transient failure keeps them; `/retry` restarts every
transient-failed job in place, under its own id: the record returns
to queued with a fresh clock and cancel flag, the pending failure
announcements retire with the state they described (a model that read
one would delegate the same task beside the re-run), and the report
cache clears, a fetch reads "still running", and the re-run
announces its finish into the id the model already knows. A re-run
that fails transiently again keeps its ingredients and restarts
again. Permanent failures never restart, re-running them repeats the
failure at full cost, and a restart past the agent's drop is an
empty answer, never a spawn into a gone agent.

**Cancellation.** A job's `ToolContext.cancelled` is its own flag,
minted at spawn, never the parent turn's. Escape flips only the
turn's flag: the main session stops, running subagents do not. No
cancel-job path exists in v1, the model cannot stop a job it
started, and the user stops one only by closing the agent; jobs are
bounded anyway (their step budget), and a stop verb over unattended
work is a later decision with its own approval question. Jobs are
bound to the agent's lifetime, not the turn's: a turn may end with
jobs running, they keep running, and the next turn's start announces
them. When the agent is dropped, running and queued jobs are
aborted and their reports are lost, the registry is in-memory, and
§6's plain-data session never carries it.

**Concurrency.** The registry runs at most 4 jobs at once (a
constructor knob, set like the nesting budget); a start beyond the
cap queues, the id is still minted and returned at once, and the
job begins when a slot frees. Queue, not reject: a rejection would
make a delegation fail on machine load the model cannot observe.
The cap counts top-level jobs only. A background job's own
`subagent` tool (when the nesting budget allows one) keeps today's
synchronous shape and runs to its report inside the job's step loop:
the notification path has exactly one steering consumer, the
driver's turn loop, and a nested job would have none.

**Approvals.** Background jobs inherit the turn's approval hook the
way foreground ones do, read off the parent shell at spawn,
installed on the job's inner shell, and the hook is captured at
spawn and held for the job's lifetime, so a gate firing after the
parent turn's end consults the turn's policy, never the fail-safe
reject. A gated call inside a background job raises the same modal,
blocking the job until it is answered, even while the parent
streams or after the parent turn has ended. Not deny-by-default:
background delegation would then be a recipe for every gated
command to fail whenever the user is not looking, and a blanket
allow would be a new unattended-execution path, exactly what the
hook exists to prevent. Denial answers in-band to the job, as to a
foreground one.

**The UI.** One pinned live line per working job. The registry
publishes each job's status, the same `ToolStatus` vocabulary the
forwarder reports today (phase, spend, model), on a subscription
`wire` hands back beside the config; the TUI pumps it beside the
engine events and keys a line by job id. A finished job's line
leaves at once, the strip pins working work only, and the
registry's finish feed hands the frontend one event per job, whose
line lands in the transcript as a muted notice, not a user bubble.
An idle session's prompt path reads as a user line instead: it is
the turn input the model received.

**Off switch.** The config's `disabled_subagents` set is the third
persisted off switch, the `skills_disabled` and `disabled_mcp_servers`
twins: the settings Subagents page flips it with space (enter stays
the edit), and discovery filters it before the catalog builds,
`wire_at` and `extra_specs` alike, so the prompt preview and the turns
agree. The built-in answers to its folder name like any definition,
and a set holding every discovered name leaves no catalog, which drops
the `subagent` tool whole and leaves the config unwired. Off rows
still list in the UI, dimmed, the skills page's rendering; the running
agent rebuilds on a toggle carrying the conversation over, so the very
next turn sees the change.

**Resume and replay.** Jobs are in-memory, so a crash mid-turn with
jobs running loses them. The committed history shows the start calls
answered with their job ids, nothing dangles, revive has nothing to
report. In the resumed session the registry is empty: a fetch of a
lost id returns `is_error` saying the job is gone, and the model
re-delegates. A start call that lost its result to the crash is
`Unsafe`: reported interrupted, never re-run.

## 14. Onboarding, bundled skills, and the no-gate chat (`rhema-code`)

The TUI's startup rule is: only the wizard gates, and only the first
time. `config.onboarded` carries the state, `None` (a config from
before the wizard, or no marker) reads as onboarded, `Some(false)`
reopens the wizard on the next start, `Some(true)` never shows it
again. `rhema onboard` forces it back on. Everything else that used to
gate, no usable backend, no stored model, opens the chat: the footer
names the missing piece in red, the submit box declines with the fix,
and `/login` or `/model` is one keystroke away. The wizard reuses the
settings' add-instance dialog inline (its validation is the model
fetch) and the palette list; steps live in `onboarding.rs`. The welcome
step introduces the flow and Enter begins it; the rail reads as
progress, with a check on passed steps, a filled dot on the current
one, and a hollow dot on the ones ahead. After the welcome the wizard
asks for the default profile: the config's profiles, `personal` and
`work` on a fresh setup, plus an add row that creates a new empty one.
The pick becomes the session's profile and the workspace's remembered
one, so the login and model steps write there; the backend re-resolves
for it, and a profile without usable credentials drops the one it left.
The finish step is the only writer of `onboarded: true`, so Esc-ing out
halfway always reopens.

**Bundled skills.** `commit-message` and `unslop` ship in the binary
(`include_str!`) and seed into the global skills directory on start,
never overwriting an existing file, and skipping disabled names. The
config's `skills_disabled` set is the only switch: the wizard's skills
step and the settings Skills page flip it, the prompt filter
(`HostAgentConfig::disabled_skills`, applied after collection and
after discovery alike) reads it, and the UIs show disabled rows greyed
rather than hidden. Re-enabling re-seeds a missing folder; a user's
folder of the same name shadows a bundled one like any other skill.
Discovery reads four tiers, from highest to lowest: the workspace's
`.rhema/skills`, the global config directory, the workspace's
`.agents/skills`, and `~/.agents/skills`. A higher tier shadows a
lower tier by name, so a skill in either rhema directory hides an
`.agents` copy of the same name, never the reverse. The two `.agents`
tiers are the cross-agent convention: the `skills` CLI
(`npx skills add`) installs project skills in the workspace's
`.agents/skills` and global skills (`-g`) in `~/.agents/skills`.
Discovery stamps each skill with the tier it came from, and the
settings page and the sandbox seeders route by that stamp, so moving a
tier's directory or adding a tier is a change to discovery alone.
Skills carry an optional frontmatter `license` (plus a `NOTICE` file
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
changes in one place, the coherence is in the calls, not in copies.

## 15. Background shells (`rhema-code/src/jobs.rs`)

The `job` bash function is a shim over an external backend (`gob`
today; `BACKEND` is the only place its name may appear): a dev
server, a watcher, a build outlives one `bash` call, and the
foreground call that waits on it burns the tool's timeout. The shim
wraps the model's line in `bash -c`, tags every job with the
session id in its description, and refuses to start a line another
session already runs (the backend's dedupe would rewrite the
description and hand one session's server to another). A machine
without a backend that can tag gets no `job` function and no prompt
section at all, rather than a command that fails at first use.

**The notification.** The prompt promises the model it will hear
when a job finishes or dies; the frontend's subscription makes it
true. The frontend holds one open subscription to the backend's
event stream (`gob events --all`) and reads it line by line,
filtering on the session tag the shim writes into every job's
description: a `job_added` or `job_started` feeds the status strip;
a `job_stopped` or `job_removed` for a job the subscription tracked
running is a finish, minted once. No timer runs. The listing is
read twice in a subscription's life, never on a tick: at the seed,
to adopt what already runs (its later end still announces) and to
leave what already ended to the past; and at every reconnect,
because the stream ends when the daemon does and the listing is
the only way to catch a finish that landed inside the gap. Between
the two, a job that starts and ends in milliseconds cannot hide,
because its two events arrive in order on one pipe. That is exactly the
race the poll this replaced could not close: a fast job ended
before the poller's first listing, the seed rule read that listing
as the past, and the finish was never minted. Detection latency is
the daemon's emit; delivery, once detected, is instant. The
subscription is the one long-lived child the frontend owns, so it
does not outlive the process that reads it: a watchdog shell
watches both pids and kills the stream when either goes, because
a crash would otherwise leave a `gob events` child subscribed to
a daemon no one reads.

Delivery is the subagent registry's two paths with one divergence,
and a finish reaches the model exactly once: the driver steers
mid-turn (a User-role message at the next step boundary, one line
per finish), or the frontend's idle tick runs the pending lines as
the next turn's prompt, never both. The registry earns exactly-once
with the fetch: the model fetching the report retires everything,
and its drain stays a query because a crash between append and
checkpoint must re-deliver an unfetched report. A job has no report
to lose, the exit code was the whole message, so here the delivery
itself retires the line: the steering drain consumes what it
returns, a steered finish never re-delivers as the idle prompt, and
a line the idle take ran is gone from the drain too. The finish
card is taken once for the transcript either way: the detection
take owns the card, the delivery take owns the line. A finish the
tick carded mid-turn that the driver never steered (minted after
the turn's last step boundary, the drain points being loop start
and each tool step) comes back after the turn ends as the idle
prompt only, its card already standing; a fresh finish cards at
delivery. Either way one card, one line to the model. The presentation:
the transcript records every finish as its own card, left-aligned
under a neutral per-palette fill, the third voice, neither the
reader's prompt (right-aligned, accent tint) nor the response (the
accent bullet); a run of finishes stacks as one block. The
vocabulary:

    Background job abc ("npm run dev") has finished (exit 0).
    Background job abc ("npm run dev") has finished with an error (exit 1).
    Background job abc ("npm run dev") was stopped.

**Two feeds, one steering slot.** The subagent registry and the job
feed chain through `SteeringChain`, subagent finishes first, job
finishes second, so the driver's single `config.steering`
coalesces both into one message in a stable order, and the idle
prompt joins them in the same order. Scoping is the session tag: a
subscription sees only its session's jobs, so a job another session
started, or a hand-run `gob add`, never announces here. A subagent
starting a job uses the shared shell, so the job carries the
parent's tag and the parent hears the finish, the subagent's own
turn steers from nothing, and that is by design: the job outlives
the subagent, the parent owns it. Headless gets the steering path
only (the one-shot has no idle tick): a job outliving the run is
the daemon's, visible to the next session only through `job ls`. A
sandbox session wires no `job` function. Its shells run elsewhere,
and it spawns no subscription.

**Self-stops.** A job the session ended through its own shim
(`job stop`, `job rm`, `job shutdown`) is no finish at all: the
model ordered the end, and the stop's own output is the
confirmation, so a notice would spend a turn telling it what it
just did. The ending verbs record the ids they stopped in a
session-scoped file under the temp dir (append-only, one short
write per record, removed when the session's handle drops); the
subscription reads the record before minting, so an ordered stop
leaves the strip silently, and a job `shutdown` failed to stop
still announces. Record and event race now that latency is the
daemon's emit: `gob stop` returns before the shim's append lands,
so an end of a tracked job re-reads the record once, a moment
later, before minting. The record is keyed by the session tag: a stop aimed at
another session's job silences nothing on that side, and a stop
from anywhere else (the user's own terminal, another session, the
kernel) records nothing and announces as before.

**Detection, exactly.** The subscription tracks the jobs whose
events it has seen, first-seen order; a stream line that fails to
parse or a listing that fails to read is skipped, never treated as
an end. A `job_stopped` gives the exit code; a `job_removed`
without one, or a tracked job found ended by the reconnect's
reconcile, finishes with none and reads as "was stopped". One
end's event tail is one finish: the `job_removed` a later `job rm`
fires finds the id already retired, and a restart's `job_started`
clears the id, because a new run deserves a finish of its own. A
job already ended when the subscription first looks (at the seed
listing, or inside a gap it never tracked) mints nothing: the
feed announces only ends this process watched or owns.
