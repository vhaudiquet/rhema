# Rhema

<div align="center">
<b>ῥῆμα, "the spoken word": words that do things.</b>

<br>

<img src=".github/img/a1-breath-dark.png" width="100">
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
See [Rhema Claw README.md](rhema-claw/README.md) for more.

## Status

- `rhema-core`, the step engine and its seams:
  - `Agent::step`: the one-exchange primitive, exact semantics in SPEC.md §4
  - `Session`: caller-owned, fully serializable; checkpointable at any step boundary
  - `ModelProvider`: the only model seam
  - `ToolResolver`: the capability seam; one primitive, not a registry
  - `Event` / `EventSink`: streaming deltas and step lifecycle events
  - `run_turn`: the provided coding-agent driver (~15 lines, not the primitive)
  - `MockProvider`: scripted offline provider, behind the `mock` feature
- `rhema-provider` is the adapters' shared base: `ModelCard` (the
  normalized catalog row), `Pricing`, the SSE decoder, and the
  `InteractiveLogin` seam adapters declare OAuth-style login flows on;
  each adapter filters its own catalog down to the text chat models it
  can drive
- `rhema-provider-google` is the Gemini adapter: streaming SSE, native
  function calling, live model listing; `GoogleProvider::new(api_key)`
  implements `ModelProvider`
- `rhema-provider-llama-cpp` is the llama.cpp `llama-server` adapter
  (OpenAI-compatible streaming chat completions): server URL, optional
  bearer key
- `rhema-provider-openrouter` is the OpenRouter adapter: the catalog's
  text chat models with per-model pricing and context lengths,
  streaming completions by API key
- `rhema-provider-anthropic` is the Anthropic Messages API adapter:
  streaming SSE, native tool use, live model listing;
  `AnthropicProvider::new(api_key)` implements `ModelProvider`
- `rhema-provider-openai-compatible` is any OpenAI-compatible
  chat-completions server (OpenAI itself, vLLM, LM Studio), addressed by
  its versioned base URL: streaming, live model listing
- `rhema-provider-copilot` is the GitHub Copilot adapter: the
  OpenAI-shaped proxy API driven by a long-lived GitHub OAuth token (the
  `InteractiveLogin` device flow's output) exchanged for a short-lived
  token the provider caches and refreshes; streaming, live model listing
- `rhema-config`, the XDG config file: named profiles of named
  provider instances (stored keys, 0600), the active provider/instance
  pair, the last-used model, and UI settings; env vars (`RHEMA_PROVIDER`,
  the per-provider key vars) always win over the stored value
- `rhema-rsd` is resource-scoped dispatch: at most one live handler per
  resource key, a live inbox delivered mid-run, revision-guarded
  checkpoints, leases with TTL and recovery, and a persistent per-key
  `Session` wrapping `rhema_core::Session`; in-memory and SQLite stores
  behind one `Store` contract
- `rhema-memory` is cross-session agent memory: one record per fact in
  global or per-workspace scope, behind a `MemoryStore` contract with an
  in-process and a markdown-file backend, surfaced as
  `memory_search`/`memory_save`/`memory_forget` tools the model calls
  itself, never injected context; the search interface hides the
  ranking method, so a semantic ranker can replace the keyword one
  without a signature change
- `rhema-identity` is the accounts layer: one record per identity the
  deployment holds on a service (a label, the service, its URL, a
  username) with one secret each, behind an `IdentityStore` contract
  with an in-process and a SQLite backend. The contract is
  use-never-read: `secret()` exists for *code* that acts with a
  credential — a forge client authenticating — and the crate ships no
  model surface that can reach it; `Account` has no secret field to
  leak
- `rhema-autonomous-agent` is the resident-agent core tier: the `Source`
  contract for deterministic model-free detection, the lease-driven
  `Driver` loop (live inbox batches, per-round checkpoints), and a tool
  registry cut into allowlist-filtered resolvers
- `rhema-claw` is the resident agent daemon: an identity (SOUL.md,
  USER.md) written with the user over a first-run onboarding offer,
  durable cross-session memory its model pulls through the `memory_*`
  tools, a chat surface over HTTP
  (native routes with SSE, plus an OpenAI-compatible `/v1/chat/
completions` shim for stock UIs), watch management by agent tool or
  HTTP, the accounts it holds on external services
  (`identity_save`/`identity_list`/`identity_forget` over chat, the
  same over `/v1/accounts`, the web UI's accounts page — the secret is
  write-only everywhere), runtime provider management over the same
  surface (log in with a kind's credentials, key or server address, or
  remove a backend; the roster re-discovers from environment, stored
  credentials, and the rhema config at every boot), the agent's
  presentation identity served over `/v1/agent` (name, mark, avatar,
  what every client renders), and forge threads answered with one
  coding-agent turn posted as the thread's reply — a watched forge
  *identity* feeds its notifications in too, triaged deterministically:
  a review request dispatches a reviewer, an assignment or feedback an
  implementer (the thread's key, session, and voice stay one; the role
  frames the turn), and a mention just surfaces to you; `--once` for
  cron-driven ephemeral machines
- `rhema-coding-agent` is the coding-agent driver: one built-in tool
  (`bash`, running commands via `bash -c` in the workspace), an
  approval seam (auto-approved for now; allowlists planned), session
  save/resume
- `rhema-acp` is the Agent Client Protocol adapter: the coding agent as
  an ACP agent subprocess, so any ACP client (Zed, JetBrains, Neovim's
  CodeCompanion, Emacs agent-shell, the VS Code extensions) drives
  rhema without a plugin per editor; prompts stream, `bash` calls ask
  the client's permission UI, `session/load` restores a saved
  conversation with a full replay
- `rhema` is the frontend binary: a full-screen terminal UI when attached
  to a terminal, `rhema run` for the pipeable one-shot, and `rhema acp`
  for editors

The crate list stays deliberately generic: integrations live outside
it. The first is `integrations/rhema-forge-integration` — GitHub,
Gitea, and GitLab clients normalizing issues/PRs/comments into one
event vocabulary, a watcher feeding them to the dispatcher, and each
account's notifications feed beside it. It is a library for any agent
consumer to compose, not a driver: the claw daemon watches repos and
answers threads through it, touching public API only.


## The claw: `rhema-claw`

The resident agent. One daemon owns every session in one SQLite store:
yours (`interactive:<name>`, keyed per chat surface) and every watched
forge thread's. Talk to it over HTTP; it watches repos, answers forge
threads with coding-agent turns, and configures its own watches when
asked in chat (`watch_repo`/`unwatch_repo`/`list_watches` are tools the
model calls like any other).

```sh
rhema-claw                     # serve on 127.0.0.1:7799 (config: listen)
rhema-claw --once              # one detect + one dispatch pass, then exit
```

For the debug loop, daemon plus UI against throwaway state and zero
credentials: `scripts/dev.sh --mock` (canned streamed replies; the UI
is at `localhost:3000`, the claw at `127.0.0.1:7799`). Drop `--mock`
and set the provider's key env var (`CLAW_DEV_PROVIDER` picks the
provider) to debug against a real model.

```yaml
# ~/.config/rhema/claw.yaml
provider: anthropic
model: claude-sonnet-4-5
listen: 127.0.0.1:7799
token: change-me                 # optional; when set, /v1/* requires it
poll_interval_secs: 120
watches:                         # a seed: imported while the table is empty
  - forge: github
    repo: acme/widgets
```

The surface refuses to start on a non-loopback `listen` with no
`token`: anything beyond loopback can reach the port, and the bearer
token is what stands between it and the agent's tools. Set a token, or
bind a loopback address. Resident sessions run the coding agent with
its approval seam auto-approved: tool calls, the shell included,
execute without a prompt. The daemon says so at startup, and the
bearer-gated surface plus the watch list are the trust boundary.

The native surface:

```
GET  /healthz                          GET  /v1/sessions
POST /v1/sessions/{key}/messages       {"text": "..."}   -> 202
GET  /v1/sessions/{key}/events         SSE turn events (see below)
GET  /v1/watches                       POST /v1/watches  DELETE /v1/watches/{forge}/{repo}
GET  /v1/bootstrap                     {"soul": bool, "user": bool}
POST /v1/ingest                         {"source","key","payload"} -> 202
GET  /v1/notifications?device=&since=   POST /v1/notifications/{id}/ack
GET  /v1/providers                      the backend instances + the active pair
PUT  /v1/providers/active               {"provider","model"?} — switch, persisted
PUT  /v1/providers/{id}/credentials     add or replace an instance, persisted
DELETE /v1/providers/{id}               remove one (the last is refused)
GET  /v1/providers/{id}/models          the live model listing
```

### Identity and first-run

The claw's prompt is assembled per turn from an in-binary base plus
three identity files under the state dir's `bootstrap/` (config:
`bootstrap_root`): **SOUL.md**, who the claw is, tone and purpose,
written with you; **USER.md**, the always-visible summary of who you
are; **ENVIRONMENT.md**, host, shell, and tool inventory, generated
deterministically at every boot (detection is code, not conversation;
hand edits are overwritten). On first run the claw *offers* a short
setup in chat: who you are, what the machine is for, who it should be.
It writes the first two files itself through the `bootstrap_save`
tool as you answer; decline and it simply keeps offering, never
interrogating. Both files stay hand-editable forever; `/v1/bootstrap`
lets a UI badge setup state.

SSE frames are JSON, `data: {..}`: `user_message`, `thinking_delta`,
`text_delta`, `tool_start`, `tool_result`, `turn_done`. A subscription
opens with the session's recorded turns replayed first and the live feed
after, cut atomically daemon-side — the record holds the newest 8192
events per key, flushed at every turn's end marker, so both a reload and
a daemon restart rebuild the transcript. A chat UI subscribes once,
POSTs messages, and resets its mirror whenever the stream (re)opens,
letting the replay rebuild it; the daemon owns the conversation, so the
client mirrors the stream rather than managing history. The OpenAI shim
and the message POST read the live feed only — replay never reaches a
turn that is starting.

Every provider whose credentials resolve (env, the claw's own stored
logins, or the stored rhema config) is constructed at boot as a backend
*instance* named by an id, the kind unless a login named it, so two
llama.cpp servers (or two keys of one cloud) coexist under two names.
One is active, and switching with `PUT /v1/providers/active` is runtime
state persisted in SQLite: the
config's `provider`/`model` pair is only the seed, and when the config
names no provider the first credential discovery actually finds (a
stored login, an env key, or the stored rhema config) is active; the
keyless llama.cpp fallback never wins a seed, so the chart pins
nothing and a switch never needs a restart.

The claw registers RSD sources three ways: watches (forge), scheduled
tasks the agent creates in chat (`schedule_task` with `at` or
`every_secs`; a due task dispatches `task:<id>` through the resident
agent, whose reply auto-notifies the phone), and a generic door,
`POST /v1/ingest`, that any external emitter addresses
(`phone:<device>:<topic>`
keys from the Android app, for example). The resident agent owns
`interactive:`, `task:`, and `phone:` keys; forge threads own the rest.
Its `notify` tool queues a notification for a device; the device polls
and acks.

Stock OpenAI-speaking UIs point at the shim instead:

```sh
curl http://127.0.0.1:7799/v1/chat/completions -H 'Content-Type: application/json' -d '{
  "model": "claw", "user": "web", "stream": true,
  "messages": [{"role": "user", "content": "what are you watching?"}]
}'
```

The shim reads only the last user message (`user` picks the session,
default `web`), maps thinking deltas to `reasoning_content`, and
streams standard chat-completions chunks. There is one notion of
session, the daemon's, behind a wire format every web UI already speaks.

## Deploying the claw

```sh
docker build -f deploy/docker/claw.Dockerfile -t rhema-claw .
helm install claw deploy/helm/rhema-claw \
  --set token=$(openssl rand -hex 24) \
  --set secret.stringData.ANTHROPIC_API_KEY=... \
  --set secret.stringData.GITHUB_TOKEN=... \
  --set config.provider=anthropic --set config.model=claude-sonnet-4-5
```

Expose it with the chart's optional ingress (`ingress.enabled` +
`ingress.host`, TLS strongly recommended, bearer tokens cross it) or
keep it cluster-private behind Tailscale; the claw is bearer-gated
either way. With the chart's UI on (`ui.enabled`, the default), the
same host serves both: the browser gets the web UI on `/`, the daemon
answers `/v1` and `/healthz`; one address for the browser and the
phone app, no CORS anywhere.

The image is the daemon plus `bash` and `git` (the agent's shell is
load-bearing); the chart runs exactly one replica, the claw is
single-node by design: one SQLite store, one dispatcher. The
store and workspaces live on a PVC, `claw.yaml` in a ConfigMap (db and
workspace paths are pinned to the mounted volume), credentials in a
Secret, and the bearer token is the `token` value, required since the
daemon binds all interfaces in-cluster. `persistence.enabled=false`
trades restart survival for disposability.

The UI in `web/claw` is one client of the same surface: `npm run dev`
proxies to a local claw, `npm run build` emits the static bundle an
ingress serves next to `/v1`. The phone is another. `android/` is the
Kotlin/Compose client: chat over the same SSE stream, phone status
(battery, charging, network, DND transitions) posted to `/v1/ingest`
as `phone:<device>:<topic>` events, and agent notifications polled
onto the lock screen (no push infrastructure; a 15-minute WorkManager
floor, live SSE while the app is open). See `android/README.md`.

## Usage

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
// one (feature `mock`). A real HTTP adapter is a future rhema-provider-* crate.
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
        .params(ChatParams::new("gemini-2.5-flash"))
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

## Tests

```sh
cargo test --workspace --all-features
```

The suite includes the RSD falsification test (SPEC.md §7 test 5): a
dispatcher holding a `Session` steps, appends an inbox item between steps,
and checkpoints through serde, expressible entirely outside crate 1.

## Roadmap

- Grow `web/claw` into the daily driver: thread transcripts, watch
  editing polish, session lifecycle (archive/delete)
- Later: an OpenClaw-style multi-channel consumer on the same dispatch
  core (Telegram as tool, not channel), memory wired into the coding
  agent and the resident tier, agent-initiated sub-sessions, compaction,
  provider fixtures via cassettes, custom base URLs for OpenAI-compatible
  servers, session compaction
