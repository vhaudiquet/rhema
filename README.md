# Rhema

ῥῆμα, "the spoken word": words that do things.

Rhema is a small, embeddable, **generic** step engine in Rust for exchanges
with language models. No coding-agent assumptions, no dispatch, no I/O baked
in. The primitive is `step`: ONE exchange, build request → stream → append
assistant message → resolve tool calls → append results. Drivers (a turn
loop, or crate 2's RSD dispatcher) decide everything else.

Spec: [SPEC.md](SPEC.md).

## Status

v1: `rhema-core`, the step engine plus its seams, plus the first consumers.

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
- `rhema-forge-agent` is the forge-watching bot: GitHub, Gitea, and
  GitLab clients normalizing issues/PRs/comments into one event
  vocabulary, a watcher feeding them to the dispatcher, and a bot that
  turns admitted keys into handler runs
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
  HTTP, runtime provider management over the same surface (log in with
  a kind's credentials, key or server address, or remove a backend;
  the roster re-discovers from environment, stored credentials, and
  the rhema config at every boot), the agent's presentation identity
  served over `/v1/agent` (name, mark, avatar, what every client
  renders), and forge threads answered with one coding-agent turn
  posted as the thread's reply; `--once` for cron-driven ephemeral
  machines
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

## The frontend: `rhema`

```sh
cargo run --bin rhema          # full-screen TUI (or `cargo install`d rhema)
```

### In your editor: `rhema acp`

```sh
rhema acp                      # serve the Agent Client Protocol over stdio
```

The agent side of ACP (agentclientprotocol.com): an editor launches
`rhema acp` as a subprocess and speaks JSON-RPC over its stdin/stdout.
Each editor session becomes a rhema session rooted at the directory the
client passes (`session/new`'s `cwd`); reply text and tool traffic
stream back as session updates, and every `bash` call asks through the
client's own permission UI. Sessions persist under the cache dir and
`session/load` restores one with a full replay.

The backend resolves once from `--profile`/`-m` and the stored config
(`RHEMA_PROVIDER` included), the same resolution the TUI and one-shot
use. On
`session/new` and `session/load`, the agent advertises a `Model` select
(`config_options`): the model it is running is the current value, in
the `<provider>/<model>` shape every choice value carries, and the
choices span every provider the profile holds credentials for. Each is
fetched over its API, its description names the provider plus the
catalog's facts (pricing, context window), and it is the same list the
TUI's `/model` picker offers. Clients that support `session/set_config_option`
use this to show a working model picker: switching the select rebuilds
the agent over the session's history, moving it across providers when
the choice names one, and the conversation continues on the new model.
Without a resolvable credential, `authenticate` fails naming the fix.

### Profiles

Config supports named profiles for contexts like work and personal. A
profile holds named **provider instances** — logged-in credentials per
provider kind — plus which provider and instance serve turns:

```yaml
profiles:
  personal:
    provider: llama-cpp
    instance: gpu box
    model: qwen3-32b
    instances:
      gpu box:
        provider: llama-cpp
        base_url: http://gpu:8080
      home:
        provider: llama-cpp
        base_url: http://localhost:8080
      copilot:
        provider: copilot
        api_key: ...
      openai:
        provider: openai
        api_key: ...
        base_url: https://api.openai.com/v1   # or any compatible server
  work:
    provider: copilot
    instance: work sub
    instances:
      work sub:
        provider: copilot
        api_key: ...
```

A profile may hold any number of instances of the same provider — two
llama.cpp servers, a personal and a work Copilot sub, several
OpenAI-compatible endpoints (an `openai` instance's `base_url` may name
any compatible server; unset means OpenAI itself). Config files written
before instances existed carry one flat credential field per provider;
loading folds those into instances named after their provider.

Without `--profile`, sessions run on the profile this workspace last
used. rhema remembers, per directory, where its last turn ran and what
`/profile` chose there. A
directory with no memory starts on `personal`, or, when personal has no
usable credentials, on the first profile that does, so a logged-in work
profile opens straight into the chat instead of the login screen.
An explicit `--profile` name is always honored as given.

Profiles are isolated: work never sees personal's instances or model.
The selection is per-instance, never a stored global switch: `rhema
--profile work` in one terminal and plain `rhema` in another run side by
side, and `/profile` switches a running TUI without touching the other
instance. Logins and model picks write into the session's profile
section, so the same provider can hold different keys per profile.
A `RHEMA_PROVIDER` environment variable overrides the profile's stored
`provider` the same way the key env vars override stored keys. That is
handy for one terminal on another backend without editing the config.

Inside the TUI, `/profile` switches mid-session: it lists the profiles
(each in its configured color, with its provider, model, and credential
state), and switching rebuilds the agent while keeping the conversation.
A profile without a usable key still switches. Prompts wait until
`/login` adds one, and the old profile's key is never borrowed.
A profile can also set `color: "#RRGGBB"` to choose the tone its name
wears in the footer and picker; unset, personal wears the jade accent and
work a burnt orange, each tuned per theme.

### The system prompt

If `~/.config/rhema/SYSTEM.md` exists, it is the base prompt every
session starts from, a [minijinja](https://docs.rs/minijinja) (Jinja)
template: `{{ workspace }}` names the session's workspace, and
`skills` lists the discovered skills, each carrying `name`,
`description`, and `path`. The template decides where, and whether,
that list appears:

```markdown
{% if skills %}
You have access to the following skills.
{% for skill in skills %}
{{ skill.name }} | {{ skill.description }} | {{ skill.path }}
{% endfor %}
{% endif %}
```

The section vanishes when no skill was found, and block tags may sit
on their own lines without blank-line runoff. A template error, bad
syntax or an unknown filter, fails loudly with the line number instead
of silently rendering half a prompt; a misspelled variable renders empty.
Without the file, rhema uses its built-in default, which carries the
same construction; nothing is ever written.

### Skills

A skill is a folder with a `SKILL.md` whose YAML frontmatter carries a
`description`, its trigger:

```yaml
---
description: apply when writing a commit message
---
Inspect the diff, then write the message as ...
```

Skills live globally under `~/.config/rhema/skills/<name>/SKILL.md`
and per-workspace under `<workspace>/.rhema/skills/`; project skills
shadow global ones by folder name. rhema presents only name, trigger,
and file in the system prompt. When a trigger applies,
the model loads the body itself with the shell (`cat`), so skills
need no special tool. `/skills` lists what was discovered.

First launch opens the login screen: one list of the profile's
logged-in instances (the active one marked) plus an `add a new
instance` row. Enter uses the highlighted instance, `d` forgets it, and
the add flow asks for a provider: Google Gemini by API key, a llama.cpp
`llama-server` by URL (optional bearer key), or
OpenRouter, Anthropic, OpenAI, and DeepSeek by API key (DeepSeek rides
the generic OpenAI-compatible adapter against their
`api.deepseek.com/v1` endpoint; `reasoning_content` streaming and
replay work natively for `deepseek-reasoner`), then a name for the
instance, then the credential (an OpenAI-compatible instance may point
its base URL at any compatible server; empty means OpenAI itself).
GitHub Copilot runs the
GitHub device flow instead: the screen shows a URL and a user code to
authorize once in the browser, the resulting long-lived GitHub token is
stored like any other key, and the adapter exchanges it for short-lived
Copilot tokens as it works. The input is validated with a
real model-list call and stored in `~/.config/rhema/config.yaml` with
`0600` permissions. Then pick a model from the live list (text chat
models only; Google rows carry pricing and context window; just type
to filter, arrows to move). The catalog spans every instance the
profile holds credentials for, each row prefixed with its provider,
and the filter matches provider and instance names too; `openrouter`
narrows the
list to OpenRouter's whole catalog. Choosing a model from another
provider or instance switches the session's backend and the choice is
stored.
`/model <text>` narrows inline instead: the input palette lists the
matching models with the same type-to-filter, arrows, and tab
completion as the command palette, and enter switches directly.
`/profile <text>` completes the config's profiles the same way.
A server exposing exactly one model
auto-selects it and skips the picker. After that you land
in chat: responses stream in as rendered markdown (headings, bold,
code in the palette's cyan, aligned tables); every tool call is a shell command,
shown as `$ command [exit-code]`, green at zero and red otherwise,
and runs auto-approved for now (esc cancels the whole turn); the
status line shows token usage per turn.

### Sessions

Leaving the TUI saves the conversation under
`~/.cache/rhema/sessions/`, grouped by the workspace it ran in.
Files are plain session JSON, the same format `--resume <file>` and
`--save <file>` take. `/resume` lists the saved sessions for the
current directory (newest first, with their age and opening prompt)
and enter continues one; a resumed session keeps writing to its own
file, so continuing never grows a second entry. A session with
nothing said saves nothing.

Pipeable one-shot mode: plain text on stdout, tool events on stderr,
non-zero exit on failure.

```sh
rhema run "summarize this diff"           # one turn, then exit
git diff | rhema run -                    # prompt from stdin
```

A piped stdin is the prompt when no argument gives one (`git diff |
rhema run`); with a terminal attached, `rhema run` without a prompt
errors instead of waiting. Bare `rhema` on a non-terminal stdout errors
too; headless runs are explicit now. Headless backends resolve from
config and environment only (there is no
login prompt), and nothing defaults: name the provider with
`RHEMA_PROVIDER` (or a stored profile), then log in once interactively
or set the key env vars: `GEMINI_API_KEY`, `OPENROUTER_API_KEY`,
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, or
`GITHUB_COPILOT_TOKEN` for the
keyed backends. Tool
calls run auto-approved for now; command allowlists are planned.

The TUI paints the Jadeite design system: jade-tinted dark ground,
one green accent, status colors reserved for meaning. `theme: light` in
config.yaml switches to the light token set for light terminals; dark
is the design's primary.

The backend is a stored decision, not a flag: the profile's `provider`
picks it (a `RHEMA_PROVIDER` environment variable overrides), and the
model comes from the picker or the profile's `model`. Nothing is
defaulted: a session with no configured provider opens the login
screen to pick one. Storing
`provider: llama-cpp` (with the server in `LLAMA_CPP_URL` or the
profile's `llama_cpp_url`) starts on the llama.cpp backend without
logging in; `ctrl-p` still lists that server's models.
Keys: `enter` sends · `shift-enter` (kitty-protocol terminals;
Ghostty's default xterm-style sequence parses through the vendored
crossterm), `alt-enter`, or `ctrl-j` adds a line, the box grows with
the text ·
a paste longer than three lines attaches: a compact chip in the box at
the paste's place (icon, index, size, erase mark; `backspace` or
`delete` removes a chip like any character), expanded in place on
submit · `ctrl-p` re-opens the model picker (the
session is kept across a model switch) · `esc` cancels the running
turn, or asks to quit when idle · `ctrl-c` or `ctrl-d` quit immediately.
While a model streams its reasoning, the status strip previews its last
rows (dimmed, three at most); the block vanishes the moment the reply
starts, and the trace is never stored in the session.
Commands: `/help`, `/clear` or
`/new` (fresh session), `/resume`, `/skills`, `/system` (the system
prompt in use), `/exit`. The transcript word-wraps and scrolls (`pgup`/
`pgdn` or mouse wheel; terminal text selection works for copy-paste).
The footer's badges are plain-Unicode icons: `⁂` names the thinking
level, and the context gauge (`◔◑◕●`) fills with the window so the fill
is readable at a glance. Terminals carrying a Nerd Font can switch to
the brain and database glyphs with `icons: nerd` in config.yaml.
Flags: `--profile`, `-m/--model`, `--workspace`, `--resume`/`--save`
(session checkpointing), `--max-steps`, `--thinking low|medium|high`
(reasoning effort; mapped
per provider: `reasoning_effort` on OpenAI-compatible endpoints (the
Copilot proxy included), `reasoning.effort` on OpenRouter, a thinking
budget on Anthropic and Gemini, and ignored where a provider has no
knob). `/thinking` changes
the level mid-session and persists it into the profile (`thinking:` in
config.yaml); the footer names the level whenever one is set.

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
bind a loopback address.

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

SSE frames are JSON, `data: {..}

`: `thinking_delta`, `text_delta`,
`tool_start`, `tool_result`, `turn_done`.
A chat UI subscribes to
`/events` once, POSTs messages, and renders the stream per turn; the
daemon owns the conversation, so the client mirrors replies rather than
managing history.

Every provider whose credentials resolve (env, the claw's own stored
logins, or the stored rhema config) is constructed at boot as a backend
*instance* named by an id, the kind unless a login named it, so two
llama.cpp servers (or two keys of one cloud) coexist under two names.
One is active, and switching with `PUT /v1/providers/active` is runtime
state persisted in SQLite: the
config's `provider`/`model` pair is only the seed, so the chart pins
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
Secret, and the bearer token is the `token` value — required, since the
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
