# Rhema

ῥῆμα — "the spoken word": words that do things.

Rhema is a small, embeddable, **generic** step engine in Rust for exchanges
with language models. No coding-agent assumptions, no dispatch, no I/O baked
in. The primitive is `step`: ONE exchange — build request → stream → append
assistant message → resolve tool calls → append results. Drivers (a turn
loop, or crate 2's RSD dispatcher) decide everything else.

Spec: [SPEC.md](SPEC.md).

## Status

v1: `rhema-core`, the step engine plus its seams, plus the first consumers.

- `rhema-core` — the step engine and its seams:
  - `Agent::step` — the one-exchange primitive, exact semantics in SPEC.md §4
  - `Session` — caller-owned, fully serializable; checkpointable at any step boundary
  - `ModelProvider` — the only model seam
  - `ToolResolver` — the capability seam; one primitive, not a registry
  - `Event` / `EventSink` — streaming deltas and step lifecycle events
  - `run_turn` — the provided coding-agent driver (~15 lines, not the primitive)
  - `MockProvider` — scripted offline provider, behind the `mock` feature
- `rhema-provider-google` — Gemini adapter: streaming SSE, native function
  calling, live model listing; `GoogleProvider::new(api_key)` implements
  `ModelProvider`
- `rhema-provider-llama-cpp` — llama.cpp `llama-server` adapter (OpenAI-
  compatible streaming chat completions): server URL, optional bearer key
- `rhema-provider-openrouter` — OpenRouter adapter: the full catalog with
  per-model pricing and context lengths, streaming completions by API key
- `rhema-provider-anthropic` — Anthropic Messages API adapter: streaming
  SSE, native tool use, live model listing; `AnthropicProvider::new(api_key)`
  implements `ModelProvider`
- `rhema-provider-openai-compatible` — any OpenAI-compatible
  chat-completions server (OpenAI itself, vLLM, LM Studio), addressed by
  its versioned base URL: streaming, live model listing
- `rhema-config` — the XDG config file: stored API key (0600) and
  last-used model; env vars always win over the stored key
- `rhema-coding-agent` — the coding-agent driver: one built-in tool
  (`bash`, running commands via `bash -c` in the workspace), an
  approval seam (auto-approved for now; allowlists planned), session
  save/resume
- `rhema` — the frontend binary: a full-screen terminal UI when attached
  to a terminal, a pipeable one-shot runner when not

## The frontend: `rhema`

```sh
cargo run --bin rhema          # full-screen TUI (or `cargo install`d rhema)
```

### Profiles

Config supports named profiles for contexts like work and personal. A
profile overrides the top-level settings field-wise, so it only needs the
fields where it differs:

```yaml
profiles:
  personal:
    provider: google
    google_api_key: ...
  work:
    provider: openrouter
    openrouter_api_key: ...
    model: anthropic/claude-sonnet-4
```

Without `--profile`, sessions run on `personal` — or, when personal has
no usable credentials, on the first profile that does, so a logged-in
work profile opens straight into the chat instead of the login screen.
An explicit `--profile` name is always honored as given.

Profiles are isolated: work never sees personal's keys or model. The
selection is per-instance, never stored — `rhema --profile work` in one
terminal and plain `rhema` in another run side by side, and `/profile`
switches a running TUI without touching the other instance. Logins and
model picks write into the session's profile section, so the same
provider can hold different keys per profile.

Inside the TUI, `/profile` switches mid-session: it lists the profiles
(each in its configured color, with its provider, model, and credential
state), and switching rebuilds the agent while keeping the conversation.
A profile without a usable key still switches — prompts wait until
`/login` adds one, and the old profile's key is never borrowed.
A profile can also set `color: "#RRGGBB"` to choose the tone its name
wears in the footer and picker; unset, personal wears the jade accent and
work the info blue.

### The system prompt

If `~/.config/rhema/SYSTEM.md` exists, it is the base prompt every
session starts from — `{workspace}` stands for the session's
workspace. Without the file, rhema uses its built-in default;
nothing is ever written. The skills table appends after either, and
the file replaces only the base — it does not remove skills.

### Skills

A skill is a folder with a `SKILL.md` whose YAML frontmatter carries a
`description` — its trigger:

```yaml
---
description: apply when writing a commit message
---
Inspect the diff, then write the message as ...
```

Skills live globally under `~/.config/rhema/skills/<name>/SKILL.md`
and per-workspace under `<workspace>/.rhema/skills/`; project skills
shadow global ones by folder name. rhema presents only a name /
trigger / file table in the system prompt — when a trigger applies,
the model loads the body itself with the shell (`cat`), so skills
need no special tool. `/skills` lists what was discovered.

First launch opens the login screen: pick a provider — Google Gemini by
API key, a llama.cpp `llama-server` by URL (optional bearer key), or
OpenRouter, Anthropic, and OpenAI by API key. The input is validated with a
real model-list call and stored in `~/.config/rhema/config.yaml` with
`0600` permissions. Then pick a model from the live list (text chat
models only — Google rows carry pricing and context window; just type
to filter, arrows to move). A server exposing exactly one model
auto-selects it and skips the picker. After that you land
in chat: responses stream in as rendered markdown (headings, bold,
code in the palette's cyan, aligned tables); every tool call is a shell command,
shown as `$ command [exit-code]` — green at zero, red otherwise —
and runs auto-approved for now (esc cancels the whole turn); the
status line shows token usage per turn.

### Sessions

Leaving the TUI saves the conversation under
`~/.cache/rhema/sessions/`, grouped by the workspace it ran in —
files are plain session JSON, the same format `--resume <file>` and
`--save <file>` take. `/resume` lists the saved sessions for the
current directory (newest first, with their age and opening prompt)
and enter continues one; a resumed session keeps writing to its own
file, so continuing never grows a second entry. A session with
nothing said saves nothing.

Pipeable one-shot mode — plain text on stdout, tool events on stderr,
non-zero exit on failure:

```sh
rhema -p "summarize this diff"            # one turn, then exit
git diff | rhema --print -                # prompt from stdin
```

Headless backends resolve from config and environment only (there is no
login prompt): log in once interactively, or set the key env vars —
`GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, or
`OPENAI_API_KEY` for the keyed backends. Tool
calls run auto-approved for now; command allowlists are planned.

The TUI paints the Jadeite design system — jade-tinted dark ground,
one green accent, status colors reserved for meaning. `--theme light`
switches to the light token set for light terminals; dark is the
design's primary.

`--provider llama-cpp --llama-url http://box:8080` starts on the llama.cpp
backend without logging in; `ctrl-p` still lists that server's models.
Keys: `enter` sends · `shift-enter` (kitty-protocol terminals),
`alt-enter`, or `ctrl-j` adds a line — the box grows with the text ·
a paste longer than three lines attaches: one chip in the box, full
content sent on submit · `ctrl-p` re-opens the model picker (the
session is kept across a model switch) · `esc` cancels the running
turn, or asks to quit when idle · `ctrl-c` or `ctrl-d` quit immediately. Commands: `/help`, `/clear` or
`/new` (fresh session), `/resume`, `/skills`, `/exit`. The transcript word-wraps and scrolls (`pgup`/
`pgdn` or mouse wheel; terminal text selection works for copy-paste).
Flags: `--workspace`, `--resume`/`--save` (session checkpointing),
`--max-steps`.

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
and checkpoints through serde — expressible entirely outside crate 1.

## Roadmap

- Crate 2: RSD `SessionManager` — dispatch, leases, inbox, revisions, SQLite
  (see [specs/resource-scoped-dispatch.md](specs/resource-scoped-dispatch.md))
- Later: compaction, provider fixtures via cassettes, custom base URLs for
  OpenAI-compatible servers
